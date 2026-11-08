//! The approval card: the question a tool call waits on, its command, its rule
//! and the row of answers.
//!
//! The card takes the text box's place, so the frame draws it between the live
//! rows and the text box. It is the mockup's bottom card with no top, bottom or
//! right edge: every row carries the warn bar `▎` in column 0, and in a rich
//! theme the warn tint under the whole width. A terminal mode has neither fill,
//! so the bar, the glyphs, the bold title and the reversed pick carry the card
//! on their own, and no color is named here: [`App::style`] answers an id.
//!
//! The card's rows are drawn from the app's own box ([`ApprovalBox`]) and from
//! the key map's [`Card`](crate::keys::Card), where the pick of the row of
//! answers and the line its answer leaves live. Drawing a row writes runs into
//! the screen's cells and builds no string: a count is written into a stack
//! buffer and the texts are the request's own.

use unicode_width::UnicodeWidthChar;

use crate::keys::Choice;
use crate::screen::{Cell, Screen};
use crate::theme::{Emphasis, Role};

use super::super::{App, ApprovalBox, Item, cut_in_place};

/// The column the card's bar takes.
const BAR: u16 = 1;

/// The columns a row of the card's body indents its text by.
const INDENT: u16 = 2;

/// The most rows of the note the card shows: the newest lines are the ones a
/// note longer than the card keeps.
const NOTE_ROWS: usize = 4;

/// The digits of the queue's count, written without allocating.
const DIGITS: usize = 4;

/// The bar of the card, warn in every mode.
const MARK: &str = "\u{258e}";

impl App {
    /// How many rows the card of the open approval needs, none when no card is
    /// on screen.
    ///
    /// It also prints the one line the answer of the last card left behind, in
    /// the write the frame it belongs to is drawn in.
    pub(crate) fn approval_rows(&mut self) -> u16 {
        self.card_sync();
        self.approval
            .as_ref()
            .map_or(0, |open| card_rows(open, self.keys.card().pick()))
    }

    /// Draws the approval card from `y` down, and answers the row after it.
    pub(crate) fn paint_approval(&mut self, y: u16) -> u16 {
        self.card_sync();
        self.card_cursor = None;
        let Some(open) = &self.approval else {
            return 0;
        };
        let width = self.text_width();
        let queued = self.queue.len();
        let pick = self.keys.card().pick();
        let inks = Inks {
            bar: self.style(Role::Warn, Emphasis::Plain),
            tint: self.style(Role::CardWarn, Emphasis::Plain),
            text: self.style(Role::Text, Emphasis::Plain),
            bold: self.style(Role::Text, Emphasis::Bold),
            dim: self.style(Role::Dim, Emphasis::Plain),
            accent: self.style(Role::Accent, Emphasis::Plain),
            code: self.style(Role::CodeFill, Emphasis::Plain),
            pick: self.style(Role::Selection, Emphasis::Plain),
            pick_key: self.style(Role::Selection, Emphasis::Bold),
        };
        let Self {
            screen, keyfile, ..
        } = self;
        let labels = keyfile.as_ref().map(crate::keyfile::KeyFile::labels);
        let (after, cursor) = draw(screen, open, pick, queued, y, width, &inks, labels);
        self.card_cursor = cursor;
        after
    }

    /// Ends the frame with the terminal's cursor on the edit point of the card's
    /// note, when the note is open, and with no cursor while it is not.
    ///
    /// The regions draw top down and the text box comes after the card, so this
    /// runs after the box has placed the cursor of its own: the note holds the
    /// one cursor of the frame while it is open, and the box while it is not.
    pub(super) fn paint_card_cursor(&mut self) {
        if let Some((x, y)) = self.card_cursor {
            self.screen.set_cursor(x, y);
        }
    }

    /// Keeps the key map's copy of the card in step with the box on screen, and
    /// prints the line the last answer left once its card is gone.
    ///
    /// The box's own rows are kept at the card's count too: full screen reserves
    /// them before the card draws, because it lays the transcript out first.
    fn card_sync(&mut self) {
        let open = self.approval.as_ref().map(|boxed| boxed.id);
        if let Some(line) = self.keys.card_mut().took(open) {
            self.answer_line(line);
        }
        let pick = self.keys.card().pick();
        let Some(boxed) = self.approval.as_mut() else {
            self.keys.card_mut().hide();
            return;
        };
        // A card with no rule to save offers no Always answer: a pick made
        // before the card's first frame steps on to No.
        let offers = !boxed.saves.is_empty();
        if !offers && pick == Choice::Always {
            self.keys.card_mut().pick_to(Choice::No);
        }
        let (id, tool, call) = (boxed.id, &boxed.tool, &boxed.call);
        self.keys.card_mut().show(id, tool, call, offers);
    }
}

impl App {
    /// Adds the one line an answer leaves to the transcript, in the role the
    /// answer names: `✓` and the add role for an allowed call, `✗` and the
    /// error role for a refused one.
    ///
    /// The tool calls that wait for their result stay in the live area: an
    /// answer names a call that has not run yet, and a frozen row would lose the
    /// output the call is about to show.
    fn answer_line(&mut self, mut line: String) {
        let width = usize::from(self.text_width());
        cut_in_place(&mut line, width);
        // The card writes the answer's glyph first (`keys::Card::took`).
        let ok = line.starts_with('\u{2713}');
        self.freeze_text();
        self.pending.push(Item::Answer { line, ok });
        self.dirty = true;
    }
}

/// The styles the card draws with, one interned id each: resolved once a frame
/// from [`App::style`], so drawing a cell costs an array read and no color math.
#[derive(Debug)]
struct Inks {
    /// The bar, the waiting glyph and the risk glyph.
    bar: u32,
    /// The warn tint under the card's rows, the screen's own style in a mode
    /// that draws no fills.
    tint: u32,
    /// Body text.
    text: u32,
    /// The title and the key of the picked choice.
    bold: u32,
    /// The details: the queue, the hint, the words around a rule.
    dim: u32,
    /// The `$` of the command and the `›` of the note.
    accent: u32,
    /// The fill behind the command, and nothing else.
    code: u32,
    /// The picked choice, and the key of it bold.
    pick: u32,
    pick_key: u32,
}

/// The rows the card needs: a padding row, the title, the command and its rule
/// under it, the row of answers, the rule the Always answer saves, the note,
/// the hint and a padding row.
fn card_rows(open: &ApprovalBox, pick: Choice) -> u16 {
    let saves = u16::from(pick == Choice::Always && !open.saves.is_empty());
    9 + saves + note_rows(open, pick)
}

/// True when the card shows the note: Tell is picked, or the note is open.
fn shows_note(open: &ApprovalBox, pick: Choice) -> bool {
    pick == Choice::Tell || open.reason.is_some()
}

/// The rows the note needs: its lines, the newest [`NOTE_ROWS`] of them.
fn note_rows(open: &ApprovalBox, pick: Choice) -> u16 {
    if !shows_note(open, pick) {
        return 0;
    }
    let Some(reason) = &open.reason else {
        return 1;
    };
    reason.split('\n').count().clamp(1, NOTE_ROWS) as u16
}

/// Draws the card from row `y` and answers the row after its last one, with the
/// edit point of its note when the note is open.
#[expect(clippy::too_many_arguments, reason = "one frame's regions, drawn once")]
fn draw(
    screen: &mut Screen,
    open: &ApprovalBox,
    pick: Choice,
    queued: usize,
    y: u16,
    width: u16,
    inks: &Inks,
    keys: Option<&crate::keyfile::Labels>,
) -> (u16, Option<(u16, u16)>) {
    let rows = card_rows(open, pick);
    let mut cursor = None;
    let last = (y + rows).min(screen.rows());
    // The warn tint goes under every row of the card, and the bar over it, in
    // column 0: no row has a top, a bottom or a right edge.
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
    // The title: the waiting glyph, the question, then how many calls wait and
    // which tool asks.
    let mut row = y + 1;
    let mut x = screen.put_str(BAR, row, "\u{23f8} ", inks.bar);
    x = screen.put_str_until(x, row, "Run this command?", inks.bold, width);
    x = screen.put_str_until(x, row, "  ", inks.text, width);
    if queued > 0 {
        let mut digits = [0u8; DIGITS];
        x = screen.put_str_until(x, row, "1 of ", inks.dim, width);
        x = screen.put_str_until(
            x,
            row,
            digits_of(queued as u64 + 1, &mut digits),
            inks.dim,
            width,
        );
        x = screen.put_str_until(x, row, " \u{b7} ", inks.dim, width);
    }
    screen.put_str_until(x, row, &open.tool, inks.dim, width);

    // The blank row, then the command as inline code, cut with a marker when it
    // is wider than the card so a long call never hides that it goes on.
    row += 2;
    x = screen.put_str_until(BAR + INDENT, row, "$", inks.accent, width);
    x = screen.put_str_until(x, row, " ", inks.code, width);
    let room = width.saturating_sub(x);
    if x + 1 < width && overflows(&open.call, room) {
        screen.put_str_until(x, row, &open.call, inks.code, width - 1);
        screen.set(width - 1, row, Cell::from_char('\u{2026}', inks.code));
    } else {
        screen.put_str_until(x, row, &open.call, inks.code, width);
    }

    // What the call runs against, as far as the request carries it: the rule
    // that asked, or the note that no saved rule matched.
    row += 1;
    x = screen.put_str_until(BAR + INDENT, row, "\u{26a0}", inks.bar, width);
    x = screen.put_str_until(x, row, " ", inks.text, width);
    if open.rule == NO_RULE {
        screen.put_str_until(x, row, "no saved rule matches", inks.dim, width);
    } else {
        x = screen.put_str_until(x, row, "asked by ", inks.dim, width);
        screen.put_str_until(x, row, &open.rule, inks.bar, width);
    }

    // The blank row, then the row of answers: each key next to its label, the
    // picked one on the selection style, its key bold.
    row += 2;
    let mut x = BAR + INDENT - 1;
    for (index, choice) in Choice::ALL.into_iter().enumerate() {
        let picked = choice == pick;
        let (case, key_style) = if picked {
            (inks.pick, inks.pick_key)
        } else {
            (inks.text, inks.text)
        };
        let (key, label) = choice.parts();
        let key = keys.map_or(key, |keys| keys.choice(choice));
        x = screen.put_str_until(x, row, " ", case, width);
        x = screen.put_str_until(x, row, key, key_style, width);
        x = screen.put_str_until(x, row, " ", case, width);
        let label_style = if picked { inks.pick } else { inks.dim };
        x = screen.put_str_until(x, row, label, label_style, width);
        x = screen.put_str_until(x, row, " ", case, width);
        if index + 1 < Choice::ALL.len() {
            x = screen.put_str_until(x, row, " ", inks.text, width);
        }
    }

    // The rule the Always answer saves, named from the request, while the pick
    // is on it: the card says what the answer writes before the user confirms.
    if pick == Choice::Always && !open.saves.is_empty() {
        row += 1;
        let mut x = screen.put_str_until(BAR + INDENT, row, "saves the rule", inks.dim, width);
        for (index, rule) in open.saves.iter().enumerate() {
            if index > 0 {
                x = screen.put_str_until(x, row, ", ", inks.dim, width);
            } else {
                x = screen.put_str_until(x, row, " ", inks.dim, width);
            }
            x = screen.put_str_until(x, row, rule, inks.text, width);
        }
        screen.put_str_until(x, row, " for this folder", inks.dim, width);
    }

    // The note the Tell answer opens: the accent arrow on its first line and
    // the newest lines of it, so the line being typed is the one on screen. Its
    // end is the frame's cursor: the note is typed where it shows.
    if shows_note(open, pick) {
        let reason = open.reason.as_deref().unwrap_or("");
        let total = reason.split('\n').count();
        let skip = total.saturating_sub(NOTE_ROWS);
        for line in reason.split('\n').skip(skip).take(NOTE_ROWS) {
            row += 1;
            let mut x = screen.put_str_until(BAR + INDENT, row, "\u{203a}", inks.accent, width);
            x = screen.put_str_until(x, row, " ", inks.text, width);
            let end = screen.put_str_until(x, row, line, inks.text, width);
            // Only the open note takes keys, so only it holds the frame's
            // cursor: the card's own keys leave the terminal's cursor hidden.
            if open.reason.is_some() {
                cursor = Some((end.min(width.saturating_sub(1)), row));
            }
        }
    }

    // The hint, and the padding row under it.
    row += 1;
    let hint = if shows_note(open, pick) {
        "enter sends your note \u{b7} esc back"
    } else {
        "\u{2190}\u{2192} choose \u{b7} enter confirm \u{b7} esc no"
    };
    screen.put_str_until(BAR + INDENT, row, hint, inks.dim, width);
    (rows, cursor)
}

/// The rule the request names when no saved rule matched it.
const NO_RULE: &str = "default";

/// True when `text` is wider than `room` columns: the columns decide, not the
/// bytes, and only the first `room + 1` of them are measured.
fn overflows(text: &str, room: u16) -> bool {
    let mut used = 0u16;
    for c in text.chars() {
        used = used.saturating_add(c.width().unwrap_or(0) as u16);
        if used > room {
            return true;
        }
    }
    false
}

/// The decimal digits of `n`, written into the end of `buf` and read as text:
/// a count costs no allocation.
fn digits_of(mut n: u64, buf: &mut [u8; DIGITS]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("1")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use reagent_protocol::message::AnswerKind;

    use crate::client::{Approval, Initialized};
    use crate::keys::Choice;
    use crate::screen::Grid;
    use crate::theme::{Emphasis, Flags, Mode, Rgb, Role};

    use super::super::super::{APPROVAL_ARMED, App, ApprovalBox, Command, Item};

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
        app
    }

    /// Puts one card of `bash` on screen, run against `rule`, the rule the card
    /// says `always` would save when `save` is set.
    fn show(app: &mut App, call: &str, rule: &str, save: bool) {
        let width = app.text_width();
        let mut boxed = ApprovalBox::new(
            Approval {
                id: 1,
                tool: "bash".into(),
                call: call.into(),
                rule: rule.into(),
                save,
            },
            width,
        );
        if save {
            boxed = boxed.saving(vec![rule.into()]);
        }
        app.approval = Some(boxed);
    }

    /// Puts a second card in the queue, so the title counts it.
    fn queue_one(app: &mut App) {
        let width = app.text_width();
        let boxed = ApprovalBox::new(
            Approval {
                id: 2,
                tool: "bash".into(),
                call: "make".into(),
                rule: "default".into(),
                save: false,
            },
            width,
        );
        app.queue.push_back(boxed);
    }

    /// Puts a card on screen, draws the frame that shows it, and ages it past
    /// [`APPROVAL_ARMED`]: the card as its keys find it.
    fn open(app: &mut App, call: &str, rule: &str, save: bool) {
        show(app, call, rule, save);
        frame(app);
        armed(app);
    }

    /// Ages the card past [`APPROVAL_ARMED`], so its keys answer it.
    fn armed(app: &mut App) {
        let older = APPROVAL_ARMED + Duration::from_millis(1);
        let shown = Instant::now()
            .checked_sub(older)
            .expect("a clock past the arming wait");
        app.approval.as_mut().expect("a card").shown = shown;
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// A keys file moves the card's answers, and the row shows the keys in
    /// force.
    #[test]
    fn custom_key_label_card_row() {
        let mut app = new_app();
        app.read_keys_text("[card]\nonce = \"o\"\n");
        open(&mut app, "make", "default", false);
        let rows = frame(&mut app);
        let answers = rows
            .iter()
            .find(|row| row.contains("Allow once"))
            .expect("the row of answers");
        assert!(
            answers.contains(" o Allow once"),
            "the answer key moved: {answers:?}"
        );
        assert!(
            !answers.contains(" y Allow once"),
            "the key it left is gone: {answers:?}"
        );
        assert!(answers.contains(" a Always here"), "{answers:?}");
    }

    /// Types `text` into whatever takes keys: the note of a card.
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.key(key(KeyCode::Char(c), NONE));
        }
    }

    /// One frame, laid out as the loop's draw lays it out: the rows the regions
    /// need are counted first, and then they draw.
    fn frame(app: &mut App) -> Vec<String> {
        app.content_rows();
        app.paint();
        rows(app)
    }

    /// The text of one painted row of the main area, trailing blanks trimmed:
    /// the sidebar's own columns are not the card's.
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

    /// The row the card's title draws on.
    fn title_row(rows: &[String]) -> usize {
        at(rows, "Run this command?").expect("the card draws")
    }

    /// The one line the transcript took from the last answer, and whether the
    /// answer allowed the call: the add role for a yes, the error role for a no.
    fn answered_line(app: &App) -> (String, bool) {
        for item in app.pending.iter().rev() {
            if let Item::Answer { line, ok } = item {
                return (line.clone(), *ok);
            }
        }
        panic!("no line of an answer: {:?}", app.pending);
    }

    /// A steady frame of the card grows nothing it holds: no cell of the grid,
    /// no style, no grapheme and no row of the box are added after the frame
    /// that warmed them up.
    #[test]
    fn the_approval_card_frame_holds_its_buffers() {
        let mut app = new_app();
        show(&mut app, "rm -rf target", "Bash(rm:*)", true);
        queue_one(&mut app);
        frame(&mut app);
        let (cells, dirty) = (
            app.screen.back().cells_capacity(),
            app.screen.back().dirty_capacity(),
        );
        let (styles, graphemes) = (app.screen.style_count(), app.screen.grapheme_count());
        let saves = app.approval.as_ref().expect("a card").saves.capacity();
        for _ in 0..8 {
            frame(&mut app);
        }
        let grid = app.screen.back();
        assert_eq!(grid.cells_capacity(), cells, "no cell is added");
        assert_eq!(grid.dirty_capacity(), dirty, "no dirty row is added");
        assert_eq!(app.screen.style_count(), styles, "no style is added");
        assert_eq!(
            app.screen.grapheme_count(),
            graphemes,
            "no grapheme is added"
        );
        assert_eq!(
            app.approval.as_ref().expect("a card").saves.capacity(),
            saves,
            "no rule of the box is added"
        );
    }

    /// The card at 120 columns: a padding row, the question with the queue and
    /// the tool, the command as inline code, the rule that asked, the row of
    /// answers and the hint, with the bar on every row.
    #[test]
    fn the_approval_card_rows_at_120_columns() {
        let mut app = new_app();
        show(&mut app, "rm -rf target", "Bash(rm:*)", true);
        queue_one(&mut app);
        let rows = frame(&mut app);
        let top = title_row(&rows);
        assert_eq!(
            &rows[top - 1..top + 8],
            [
                "\u{258e}",
                "\u{258e}\u{23f8} Run this command?  1 of 2 \u{b7} bash",
                "\u{258e}",
                "\u{258e}  $ rm -rf target",
                "\u{258e}  \u{26a0} asked by Bash(rm:*)",
                "\u{258e}",
                "\u{258e}  y Allow once   a Always here   n No   t Tell it instead",
                "\u{258e}  \u{2190}\u{2192} choose \u{b7} enter confirm \u{b7} esc no",
                "\u{258e}",
            ],
            "\n{}",
            rows.join("\n")
        );
        // One card alone names the tool and counts nothing.
        {
            let mut alone = new_app();
            show(&mut alone, "rm -rf target", "Bash(rm:*)", true);
            let rows = frame(&mut alone);
            assert!(
                rows[title_row(&rows)].ends_with("Run this command?  bash"),
                "the title names the tool: {:?}",
                rows[title_row(&rows)]
            );
        }
        // A request with no rule shows what exists instead of a rule.
        {
            let mut bare = new_app();
            show(&mut bare, "make", "default", true);
            let rows = frame(&mut bare);
            assert!(
                rows[title_row(&rows) + 3].ends_with("\u{26a0} no saved rule matches"),
                "no rule matched: {:?}",
                rows[title_row(&rows) + 3]
            );
        }
    }

    /// Left and Right move the pick and stop at the ends, and the row draws the
    /// pick it holds on the selection style.
    #[test]
    fn the_approval_card_pick_moves_and_stops_at_the_ends() {
        let mut app = new_app();
        open(&mut app, "rm -rf target", "Bash(rm:*)", true);
        let right = key(KeyCode::Right, NONE);
        let left = key(KeyCode::Left, NONE);
        for want in [Choice::Always, Choice::No, Choice::Tell, Choice::Tell] {
            app.key(right);
            assert_eq!(app.keys.card().pick(), want, "Right stops at the end");
        }
        for want in [Choice::No, Choice::Always, Choice::Once, Choice::Once] {
            app.key(left);
            assert_eq!(app.keys.card().pick(), want, "Left stops at the start");
        }
        // The picked label is the one drawn reversed: the text on the selection
        // style, with nothing chosen while Once is picked but its key.
        let rows = frame(&mut app);
        let answers = at(&rows, "Allow once").expect("the row of answers draws");
        let grid = app.screen.back();
        let selection = app.style(Role::Selection, Emphasis::Plain);
        let label = rows[answers].find("Allow once").expect("the label") as u16;
        assert_eq!(grid.cell(label, answers as u16).style(), selection);
        app.key(right);
        // The rule row of the Always answer takes a row, so the row of answers
        // moves up one: it is found again in the frame it is drawn in.
        let rows = frame(&mut app);
        let answers = at(&rows, "Always here").expect("the row of answers draws");
        let grid = app.screen.back();
        let label = rows[answers].find("Always here").expect("the label") as u16;
        assert_eq!(
            grid.cell(label, answers as u16).style(),
            selection,
            "the pick moved to Always here"
        );
        assert_eq!(
            grid.cell(
                rows[answers].find("Allow once").expect("the label") as u16,
                answers as u16
            )
            .style(),
            app.style(Role::Dim, Emphasis::Plain),
            "and left the one it was on"
        );
    }

    /// Enter answers with the pick: once, always, no, and the note for Tell it
    /// instead, which answers the card when the note goes.
    #[test]
    fn the_approval_card_enter_answers_with_the_pick() {
        let enter = key(KeyCode::Enter, NONE);
        for (steps, want) in [
            (0, Some(AnswerKind::Once)),
            (1, Some(AnswerKind::Save)),
            (2, Some(AnswerKind::Refuse)),
            (3, None),
        ] {
            let mut app = new_app();
            show(&mut app, "rm -rf target", "Bash(rm:*)", true);
            armed(&mut app);
            for _ in 0..steps {
                app.key(key(KeyCode::Right, NONE));
            }
            app.key(enter);
            match want {
                Some(answer) => {
                    let command = app.commands.pop_front().expect("the answer goes out");
                    assert!(
                        matches!(command, Command::Approve { id: 1, answer: got, reason: None } if got == answer),
                        "{steps} steps from Once answered {command:?}, not {answer:?}"
                    );
                    assert!(app.approval.is_none(), "the card is answered");
                }
                None => {
                    assert!(app.commands.is_empty(), "Tell it instead sends nothing yet");
                    let open = app.approval.as_ref().expect("the card stays");
                    assert_eq!(open.reason.as_deref(), Some(""), "the note is open");
                }
            }
        }
        // The letters answer at once, without moving the pick first.
        for (pressed, want) in [
            ('y', AnswerKind::Once),
            ('a', AnswerKind::Save),
            ('n', AnswerKind::Refuse),
        ] {
            let mut app = new_app();
            show(&mut app, "rm -rf target", "Bash(rm:*)", true);
            armed(&mut app);
            app.key(key(KeyCode::Char(pressed), NONE));
            let command = app.commands.pop_front().expect("the answer goes out");
            assert!(
                matches!(command, Command::Approve { answer, .. } if answer == want),
                "{pressed:?} answered {command:?}, not {want:?}"
            );
        }
        // Esc is the refusal with no note.
        let mut app = new_app();
        open(&mut app, "rm -rf target", "Bash(rm:*)", true);
        app.key(key(KeyCode::Esc, NONE));
        let command = app.commands.pop_front().expect("the answer goes out");
        assert!(matches!(
            command,
            Command::Approve {
                answer: AnswerKind::Refuse,
                reason: None,
                ..
            }
        ));
    }

    /// When Always is picked the card writes what the answer does with the rule,
    /// and a card with no rule to save offers it not at all.
    #[test]
    fn the_approval_card_shows_the_rule_always_saves() {
        let mut app = new_app();
        open(&mut app, "rm -rf target", "Bash(rm:*)", true);
        app.key(key(KeyCode::Right, NONE));
        let rows = frame(&mut app);
        let answers = at(&rows, "Always here").expect("the row of answers draws");
        assert_eq!(
            rows[answers + 1],
            "\u{258e}  saves the rule Bash(rm:*) for this folder",
            "the rule row follows the row of answers:\n{}",
            rows.join("\n")
        );
        // The words are dim, and the rule the answer writes is body text.
        let words = rows[answers + 1].find("saves").expect("the words") as u16;
        assert_eq!(
            app.screen.back().cell(words, answers as u16 + 1).style(),
            app.style(Role::Dim, Emphasis::Plain)
        );
        let rule = rows[answers + 1].find("Bash").expect("the rule") as u16;
        assert_eq!(
            app.screen.back().cell(rule, answers as u16 + 1).style(),
            app.style(Role::Text, Emphasis::Plain),
            "the rule the answer saves is named in body text"
        );

        // No rule to save: the pick steps over Always and no rule row shows.
        {
            let mut bare = new_app();
            open(&mut bare, "rm -rf target", "default", false);
            bare.key(key(KeyCode::Right, NONE));
            assert_eq!(bare.keys.card().pick(), Choice::No);
            let rows = frame(&mut bare);
            assert!(at(&rows, "saves").is_none(), "\n{}", rows.join("\n"));
        }
    }

    /// Tell it instead opens a note in the card: Shift+Enter starts a line of
    /// it, Enter sends it as the answer, and its lines are the card's rows.
    #[test]
    fn the_approval_card_opens_a_note_for_tell_it_instead() {
        let mut app = new_app();
        open(&mut app, "rm -rf target", "default", true);
        app.key(key(KeyCode::Char('t'), NONE));
        assert_eq!(app.keys.card().pick(), Choice::Tell);
        let rows = frame(&mut app);
        assert_eq!(
            rows[title_row(&rows) + 7],
            "\u{258e}  enter sends your note \u{b7} esc back",
            "the hint turns to the note's:\n{}",
            rows.join("\n")
        );
        assert!(
            rows[title_row(&rows) + 6].starts_with("\u{258e}  \u{203a}"),
            "the note opens with its arrow: {:?}",
            rows[title_row(&rows) + 6]
        );

        // Tell it instead opens the note with the answer, so the keys type it.
        assert_eq!(
            app.approval.as_ref().expect("the card").reason.as_deref(),
            Some(""),
            "the note is open"
        );
        type_text(&mut app, "use cargo clean");
        app.key(key(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut app, "then retry");
        let open = app.approval.as_ref().expect("the card stays");
        assert_eq!(
            open.reason.as_deref(),
            Some("use cargo clean\nthen retry"),
            "Shift+Enter starts the second line"
        );
        let rows = frame(&mut app);
        let first = at(&rows, "use cargo clean").expect("the note draws");
        assert!(
            rows[first + 1].ends_with("then retry"),
            "the second line draws under the first:\n{}",
            rows.join("\n")
        );

        app.key(key(KeyCode::Enter, NONE));
        let command = app.commands.pop_front().expect("the note answers the card");
        assert!(
            matches!(
                command,
                Command::Approve {
                    answer: AnswerKind::Refuse,
                    reason: Some(ref note),
                    ..
                } if note == "use cargo clean\nthen retry"
            ),
            "the note goes with the refusal: {command:?}"
        );
    }

    /// After the answer the transcript keeps one line, the answer and the call
    /// the card asked about, and it is printed once.
    #[test]
    fn the_approval_card_leaves_one_line_after_its_answer() {
        let mut app = new_app();
        open(&mut app, "rm -rf target", "default", true);
        app.key(key(KeyCode::Char('y'), NONE));
        assert!(app.approval.is_none(), "the answer closes the card");
        frame(&mut app);
        let (line, ok) = answered_line(&app);
        assert_eq!(line, "\u{2713} allowed once \u{b7} bash rm -rf target");
        assert!(ok, "a yes allows the call");
        frame(&mut app);
        assert_eq!(app.pending.len(), 1, "the line is printed once");

        {
            let mut denied = new_app();
            open(&mut denied, "rm -rf target", "default", true);
            denied.key(key(KeyCode::Esc, NONE));
            frame(&mut denied);
            let (line, ok) = answered_line(&denied);
            assert_eq!(line, "\u{2717} denied \u{b7} bash rm -rf target");
            assert!(!ok, "a no refuses the call");
        }
    }

    /// The note holds the frame's one cursor: the terminal's own cursor ends
    /// the frame on the note's edit point, and no cell stands in for it.
    #[test]
    fn the_approval_note_holds_the_frames_cursor() {
        let mut app = new_app();
        open(&mut app, "rm -rf target", "default", true);
        app.key(key(KeyCode::Char('t'), NONE));
        type_text(&mut app, "use cargo clean");
        let rows = frame(&mut app);
        let note = at(&rows, "use cargo clean").expect("the note draws");
        // The cursor sits one column past the note's last cell, which is the
        // end of its row: the arrow, its space and the text.
        let end = u16::try_from(rows[note].chars().count()).expect("a column");
        let place = format!("\x1b[{};{}H\x1b[?25h", note + 1, end + 1);
        let bytes = frame_bytes(&mut app);
        assert_eq!(
            bytes.matches("\x1b[?25h").count(),
            1,
            "one cursor in the frame: {bytes:?}"
        );
        assert!(
            bytes.ends_with(&format!("{place}\x1b[?2026l")),
            "the frame ends on the note's edit point {place:?}: {bytes:?}"
        );

        // Back on the choices the note is gone and the box holds the cursor.
        app.key(key(KeyCode::Esc, NONE));
        let rows = frame(&mut app);
        assert!(at(&rows, "use cargo clean").is_none(), "the note closed");
        let bytes = frame_bytes(&mut app);
        assert!(
            !bytes.contains("\x1b[?25h"),
            "the card without its note draws no cursor: {bytes:?}"
        );
    }

    /// Esc in the note goes back to the row of choices: it closes the note and
    /// refuses nothing, so the card waits for another answer.
    #[test]
    fn esc_in_the_note_goes_back_to_the_choices() {
        let mut app = new_app();
        open(&mut app, "rm -rf target", "default", true);
        app.key(key(KeyCode::Char('t'), NONE));
        type_text(&mut app, "use cargo clean");
        app.key(key(KeyCode::Esc, NONE));
        let open = app.approval.as_ref().expect("the card stays");
        assert_eq!(open.reason, None, "the note is closed");
        assert_eq!(
            app.keys.card().pick(),
            Choice::Tell,
            "the row of choices keeps its pick"
        );
        assert!(app.commands.is_empty(), "no answer went out");

        // A key on the card refuses it, with no reason and no note.
        armed(&mut app);
        app.key(key(KeyCode::Esc, NONE));
        let command = app.commands.pop_front().expect("the answer goes out");
        assert!(
            matches!(
                command,
                Command::Approve {
                    answer: AnswerKind::Refuse,
                    reason: None,
                    ..
                }
            ),
            "the card refuses it: {command:?}"
        );
    }

    /// The line an answer leaves in the history draws in the role of the
    /// answer: the add role of a yes, the error role of a no.
    #[test]
    fn the_approval_answer_line_draws_in_the_role_of_the_answer() {
        let mut app = new_app();
        open(&mut app, "rm -rf target", "default", true);
        app.key(key(KeyCode::Char('y'), NONE));
        // The frame that takes the line into `pending`, the commit that prints
        // it into the transcript, and the frame that draws it.
        frame(&mut app);
        app.commit();
        let rows = frame(&mut app);
        let line = at(&rows, "allowed once").expect("the line of the answer draws");
        let add = app.style(Role::Add, Emphasis::Plain);
        let mark = rows[line].find('\u{2713}').expect("the answer's glyph") as u16;
        assert_eq!(
            app.screen.back().cell(mark, line as u16).style(),
            add,
            "a yes allows the call in the add role"
        );

        let mut denied = new_app();
        open(&mut denied, "rm -rf target", "default", true);
        denied.key(key(KeyCode::Esc, NONE));
        frame(&mut denied);
        denied.commit();
        let rows = frame(&mut denied);
        let line = at(&rows, "denied").expect("the line of the answer draws");
        let error = denied.style(Role::Error, Emphasis::Plain);
        let mark = rows[line].find('\u{2717}').expect("the answer's glyph") as u16;
        assert_eq!(
            denied.screen.back().cell(mark, line as u16).style(),
            error,
            "a no refuses the call in the error role"
        );
    }

    /// The bytes one frame writes, as the loop writes them.
    fn frame_bytes(app: &mut App) -> String {
        let mut bytes = Vec::new();
        app.paint();
        app.paint_input_fill();
        app.screen.diff(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// In the terminal mode the card draws no tint and no code fill, and the bar
    /// and the glyphs carry it: the region still reads with no fills.
    #[test]
    fn the_approval_card_in_terminal_mode_has_no_tint_but_keeps_its_bar() {
        let mut app = new_app();
        show(&mut app, "rm -rf target", "default", true);
        let rows = frame(&mut app);
        let top = title_row(&rows);
        let tint = app.style(Role::CardWarn, Emphasis::Plain);
        assert_eq!(
            tint,
            app.screen.default_style(),
            "the terminal mode draws no card tint"
        );
        let grid = app.screen.back();
        let right = app.main.width - 1;
        for y in top as u16 - 1..=top as u16 + 7 {
            assert!(
                row(grid, y, app.main.width).starts_with('\u{258e}'),
                "row {y} carries the bar: {:?}",
                row(grid, y, app.main.width)
            );
            assert_eq!(
                grid.cell(0, y).style(),
                app.style(Role::Warn, Emphasis::Plain),
                "the bar is the warn role"
            );
            assert_eq!(
                grid.cell(right, y).style(),
                app.screen.default_style(),
                "no fill under row {y}"
            );
        }
        assert!(
            row(grid, top as u16, app.main.width).contains("\u{23f8} Run this command?"),
            "the glyph and the title carry the question"
        );
    }

    /// In a rich theme the warn tint fills the card's rows and the command sits
    /// on the code fill, behind its own text only.
    #[test]
    fn the_approval_card_tints_its_rows_in_a_rich_theme() {
        let mut app = new_app();
        app.restyle(Mode::Rich {
            background: DARK,
            dark: true,
        });
        let tint = app.style(Role::CardWarn, Emphasis::Plain);
        if tint == app.screen.default_style() {
            // `NO_COLOR` in the test environment: no mode draws a fill at all.
            assert!(!Flags::from_env().color, "the rich theme fills the card");
            return;
        }
        show(&mut app, "rm -rf target", "default", true);
        let rows = frame(&mut app);
        let top = title_row(&rows);
        let grid = app.screen.back();
        let right = app.main.width - 1;
        assert_eq!(grid.cell(right, top as u16 - 1).style(), tint);
        assert_eq!(grid.cell(right, top as u16).style(), tint);
        let call = at(&rows, "rm -rf target").expect("the command draws") as u16;
        let x = rows[usize::from(call)].find("rm -rf").expect("its column") as u16;
        assert_eq!(
            grid.cell(x, call).style(),
            app.style(Role::CodeFill, Emphasis::Plain),
            "the command's text is on the code fill"
        );
        assert_eq!(
            grid.cell(right, call).style(),
            tint,
            "the code fill stops with the text"
        );
    }
}
