//! Drawing the list above the text box: the commands, the files and the
//! leader's keys.
//!
//! The list takes the width of the main column from column 0 and never writes
//! past it. Every row is filled with the list's own fill first, so the list
//! reads as one band on the surface under the text box; the picked row takes
//! the picked fill in its place, and every ink it writes with carries the
//! background of the fill it is on, so a gap between a name and what it does
//! shows the fill. [`draw_list`] takes a [`Screen`] and the list alone, with no
//! [`App`](crate::app::App): the frame that owns the region answers the inks
//! from the theme and passes them in.
//!
//! Rows, top down: a blank one; in the palette a query row and a group row
//! before each group; one row per item, a marker in column 0, the name with the
//! matched chars marked and the key at the right edge; the leader's title and
//! its grid of keys; and a footer that names the keys of the list. A count goes
//! through a stack buffer, so drawing allocates nothing.

use unicode_width::UnicodeWidthStr;

use crate::keys::{self, Group};
use crate::screen::{Cell, Screen};

use super::super::list::{Command, List, PER_ROW, Source};

/// The column the words of a command start at: the name and its padding take
/// the columns before it.
const NAME_COL: u16 = 14;

/// The column a group row of the palette starts at.
const GROUP_COL: u16 = 2;

/// The column the footer starts at.
const FOOT_COL: u16 = 2;

/// The columns one key of the leader grid is right-aligned in.
const KEY_COL: u16 = 6;

/// The columns the words after a key of the leader grid are padded to.
const WORDS_COL: u16 = 16;

/// The columns one cell of the leader grid takes: its key, a space and its
/// words.
const CELL_COL: u16 = KEY_COL + 1 + WORDS_COL;

/// The digits of a count, written without allocating.
const DIGITS: usize = 8;

/// The glyph of the picked row.
const MARK: &str = "\u{203a}";

/// The footer of the `/` list.
const COMMANDS_FOOT: &str =
    "\u{2191}\u{2193} choose \u{b7} tab completes \u{b7} enter runs \u{b7} esc closes";

/// The footer of the palette.
const PALETTE_FOOT: &str = "ctrl+x / closes \u{b7} your draft stays in the box";

/// The footer of the file list.
const FILES_FOOT: &str = " files \u{b7} tab inserts the path \u{b7} esc closes";

/// The footer of the leader list.
const LEADER_FOOT: &str = "esc cancels \u{b7} ? shows every key";

/// The two inks of a name: the chars the query matched and the ones it did not.
#[derive(Debug, Clone, Copy)]
struct Name {
    plain: u32,
    mark: u32,
}

/// The styles one row of the list draws with, one interned id each: resolved
/// once a frame, so drawing a cell costs an array read and no color math.
#[derive(Debug)]
pub(crate) struct RowInks {
    /// The fill under the row.
    pub fill: u32,
    /// Body text: a name, the palette's query.
    pub text: u32,
    /// The name of the picked row and the words of the leader grid.
    pub bold: u32,
    /// What a command does, its key and a footer.
    pub dim: u32,
    /// The marker, the matched chars and the keys of the leader grid, in the
    /// accent and bold.
    pub mark: u32,
}

/// The styles the list draws with: the rows on the list's fill and the picked
/// row on its own.
#[derive(Debug)]
pub(crate) struct ListInks {
    /// The rows on the list's fill.
    pub plain: RowInks,
    /// The picked row, on its own fill.
    pub picked: RowInks,
}

/// Draws `list` from row `y`, `width` columns wide from column 0, and answers
/// the rows it drew.
pub(crate) fn draw_list(
    screen: &mut Screen,
    list: &List,
    y: u16,
    width: u16,
    inks: &ListInks,
) -> u16 {
    let rows = list.height().min(screen.rows().saturating_sub(y));
    for row in y..y + rows {
        fill(screen, row, width, inks.plain.fill);
    }
    if rows < 2 {
        return rows;
    }
    let footer = y + rows - 1;
    match list.source() {
        Source::Commands(_) if list.is_palette() => {
            palette(screen, list, y + 1, footer, width, inks)
        }
        Source::Commands(_) => commands(screen, list, y + 1, footer, width, inks),
        Source::Files(_) => files(screen, list, y + 1, footer, width, inks),
        Source::Leader => leader(screen, y + 1, footer, width, inks),
    }
    footer_row(screen, list, footer, width, inks);
    rows
}

/// Draws the rows of the `/` list: one row per hit, the picked one marked.
fn commands(
    screen: &mut Screen,
    list: &List,
    mut row: u16,
    footer: u16,
    width: u16,
    inks: &ListInks,
) {
    for hit in list.window() {
        if row >= footer {
            return;
        }
        let Some(command) = list.command_at(hit) else {
            continue;
        };
        let picked = hit == list.selected();
        command_row(screen, command, list.marks(hit), picked, row, width, inks);
        row += 1;
    }
}

/// Draws the rows of the palette: its query row, a group row before each group
/// while the query is empty, and one row per hit.
fn palette(
    screen: &mut Screen,
    list: &List,
    mut row: u16,
    footer: u16,
    width: u16,
    inks: &ListInks,
) {
    let plain = &inks.plain;
    if row < footer {
        query_row(screen, list, row, width, plain);
        row += 1;
    }
    let groups = list.query().is_empty();
    let mut previous = None;
    for hit in list.window() {
        if row >= footer {
            return;
        }
        let Some(command) = list.command_at(hit) else {
            continue;
        };
        if groups && previous != Some(command.group) {
            screen.put_str_until(GROUP_COL, row, command.group, plain.dim, width);
            previous = Some(command.group);
            row += 1;
            if row >= footer {
                return;
            }
        }
        let picked = hit == list.selected();
        command_row(screen, command, list.marks(hit), picked, row, width, inks);
        row += 1;
    }
}

/// Draws the palette's query row: the marker, then the query or what typing
/// filters.
fn query_row(screen: &mut Screen, list: &List, row: u16, width: u16, inks: &RowInks) {
    let mut x = screen.put_str_until(0, row, MARK, inks.mark, width);
    x = screen.put_str_until(x, row, " ", inks.fill, width);
    if !list.query().is_empty() {
        screen.put_str_until(x, row, list.query(), inks.text, width);
        return;
    }
    let mut buf = [0u8; DIGITS];
    x = screen.put_str_until(x, row, "type to filter ", inks.dim, width);
    x = screen.put_str_until(
        x,
        row,
        digits_of(list.total() as u64, &mut buf),
        inks.dim,
        width,
    );
    screen.put_str_until(x, row, " commands", inks.dim, width);
}

/// Draws the rows of the file list: one row per hit, the path marked where the
/// query matched, the picked one marked.
fn files(screen: &mut Screen, list: &List, mut row: u16, footer: u16, width: u16, inks: &ListInks) {
    for hit in list.window() {
        if row >= footer {
            return;
        }
        let Some(path) = list.path_at(hit) else {
            continue;
        };
        let picked = hit == list.selected();
        let inks = if picked { &inks.picked } else { &inks.plain };
        let name = Name {
            plain: if picked { inks.bold } else { inks.text },
            mark: inks.mark,
        };
        let x = marker(screen, row, picked, width, inks);
        marked(screen, x, row, path, list.marks(hit), name, width);
        row += 1;
    }
}

/// Draws the leader's title row and its grid of keys, [`PER_ROW`] to a row.
fn leader(screen: &mut Screen, mut row: u16, footer: u16, width: u16, inks: &ListInks) {
    let inks = &inks.plain;
    if row >= footer {
        return;
    }
    let mut x = screen.put_str_until(0, row, "ctrl+x", inks.mark, width);
    x = screen.put_str_until(x, row, " ", inks.fill, width);
    screen.put_str_until(x, row, "then", inks.dim, width);
    row += 1;
    let mut cell = 0;
    let mut x = 1;
    for hint in keys::hints(Group::Leader) {
        if row >= footer {
            return;
        }
        if cell == PER_ROW {
            cell = 0;
            x = 1;
            row += 1;
            if row >= footer {
                return;
            }
        }
        let keys = (UnicodeWidthStr::width(hint.keys) as u16).min(KEY_COL);
        let key_x = x + KEY_COL - keys;
        screen.put_str_until(key_x, row, hint.keys, inks.mark, width);
        screen.put_str_until(x + KEY_COL + 1, row, hint.words, inks.text, width);
        x += CELL_COL;
        cell += 1;
    }
}

/// Draws one command: the marker, the name and its padding, what it does and
/// the key that runs it elsewhere at the right edge.
fn command_row(
    screen: &mut Screen,
    command: &Command,
    marks: &[u16],
    picked: bool,
    row: u16,
    width: u16,
    inks: &ListInks,
) {
    let inks = if picked { &inks.picked } else { &inks.plain };
    if picked {
        fill(screen, row, width, inks.fill);
    }
    let mut x = marker(screen, row, picked, width, inks);
    // The picked name is bold as a whole; every other one marks the chars the
    // query matched.
    let (name, marks) = if picked {
        (
            Name {
                plain: inks.bold,
                mark: inks.bold,
            },
            &[][..],
        )
    } else {
        (
            Name {
                plain: inks.text,
                mark: inks.mark,
            },
            marks,
        )
    };
    x = marked(screen, x, row, command.name, marks, name, width);
    while x < NAME_COL && x < width {
        screen.set(x, row, Cell::from_char(' ', inks.fill));
        x += 1;
    }
    x = screen.put_str_until(x, row, command.words, inks.dim, width);
    if command.key.is_empty() || width < 2 {
        return;
    }
    let keys = (UnicodeWidthStr::width(command.key) as u16).min(width - 1);
    let key_x = width - 1 - keys;
    if key_x > x {
        // The row keeps one column after the key, so it never reaches the edge.
        screen.put_str_until(key_x, row, command.key, inks.dim, width - 1);
    }
}

/// Draws the footer of the list, in the words of its source.
fn footer_row(screen: &mut Screen, list: &List, row: u16, width: u16, inks: &ListInks) {
    let inks = &inks.plain;
    let mut buf = [0u8; DIGITS];
    let mut x = FOOT_COL;
    match list.source() {
        Source::Commands(_) if list.is_palette() => {
            screen.put_str_until(x, row, PALETTE_FOOT, inks.dim, width);
        }
        Source::Commands(_) => {
            screen.put_str_until(x, row, COMMANDS_FOOT, inks.dim, width);
        }
        Source::Files(_) => {
            x = screen.put_str_until(
                x,
                row,
                digits_of(list.hits() as u64, &mut buf),
                inks.dim,
                width,
            );
            x = screen.put_str_until(x, row, " of ", inks.dim, width);
            x = screen.put_str_until(
                x,
                row,
                digits_of(list.total() as u64, &mut buf),
                inks.dim,
                width,
            );
            screen.put_str_until(x, row, FILES_FOOT, inks.dim, width);
        }
        Source::Leader => {
            screen.put_str_until(x, row, LEADER_FOOT, inks.dim, width);
        }
    }
}

/// Fills one row with `style` across `width` columns.
fn fill(screen: &mut Screen, row: u16, width: u16, style: u32) {
    let width = width.min(screen.columns());
    for x in 0..width {
        screen.set(x, row, Cell::from_char(' ', style));
    }
}

/// Draws the marker of a row, `›` on the picked one, and answers the column
/// after it.
fn marker(screen: &mut Screen, row: u16, picked: bool, width: u16, inks: &RowInks) -> u16 {
    let (glyph, ink) = if picked {
        (MARK, inks.mark)
    } else {
        (" ", inks.fill)
    };
    let x = screen.put_str_until(0, row, glyph, ink, width);
    screen.put_str_until(x, row, " ", inks.fill, width)
}

/// Writes `text` at `x` with the chars at `marks` in the name's mark ink and the
/// rest in its plain ink, and answers the column after it.
fn marked(
    screen: &mut Screen,
    x: u16,
    row: u16,
    text: &str,
    marks: &[u16],
    name: Name,
    width: u16,
) -> u16 {
    let mut x = x;
    let mut marks = marks.iter().copied();
    let mut next = marks.next();
    let mut buf = [0u8; 4];
    for (index, c) in text.chars().enumerate() {
        let hit = next == Some(index as u16);
        if hit {
            next = marks.next();
        }
        let style = if hit { name.mark } else { name.plain };
        x = screen.put_str_until(x, row, c.encode_utf8(&mut buf), style, width);
    }
    x
}

/// The decimal digits of `n`, written into the end of `buf` and read as text: a
/// count costs no allocation.
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
    std::str::from_utf8(&buf[at..]).unwrap_or("0")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::screen::{Cell, Color, Screen, Style};

    use super::super::super::list::{Command, List};
    use super::{ListInks, RowInks, draw_list};

    /// The columns and the rows of the test screen.
    const COLUMNS: u16 = 80;
    const ROWS: u16 = 24;

    /// A screen of the test terminal.
    fn screen() -> Screen {
        Screen::new(COLUMNS, ROWS)
    }

    /// The fills of the tests: the list's own and the picked row's.
    const FILL: Color = Color::Rgb(0x20, 0, 0);
    const PICKED: Color = Color::Rgb(0x30, 0, 0);

    /// The inks of one fill: a distinct foreground per role over it, and the
    /// fill itself for the blank cells.
    fn row_inks(screen: &mut Screen, bg: Color, base: u8) -> RowInks {
        fn ink(screen: &mut Screen, bg: Color, fg: u8, emphasis: Style) -> u32 {
            screen.intern(Style {
                fg: Color::Rgb(fg, 0, 0),
                bg,
                ..emphasis
            })
        }
        RowInks {
            fill: screen.intern(Style {
                bg,
                ..Style::default()
            }),
            text: ink(screen, bg, base + 1, Style::default()),
            bold: ink(
                screen,
                bg,
                base + 2,
                Style {
                    bold: true,
                    ..Style::default()
                },
            ),
            dim: ink(
                screen,
                bg,
                base + 3,
                Style {
                    dim: true,
                    ..Style::default()
                },
            ),
            mark: ink(
                screen,
                bg,
                base + 4,
                Style {
                    bold: true,
                    ..Style::default()
                },
            ),
        }
    }

    /// The inks and the fills the list draws with, one distinct style each.
    fn inks(screen: &mut Screen) -> ListInks {
        ListInks {
            plain: row_inks(screen, FILL, 0x01),
            picked: row_inks(screen, PICKED, 0x11),
        }
    }

    /// The four commands the list tests draw.
    fn commands() -> Arc<[Command]> {
        [
            (
                "/compact",
                "summarize the session to free context",
                "",
                "session",
            ),
            (
                "/model",
                "pick the model and the provider",
                "ctrl+x m",
                "model",
            ),
            ("/mode", "default, plan or yolo", "shift+tab", "model"),
            ("/cost", "tokens and cost of this session", "", "session"),
        ]
        .into_iter()
        .map(|(name, words, key, group)| Command {
            name,
            words,
            key,
            group,
        })
        .collect::<Vec<_>>()
        .into()
    }

    /// Three commands with one group each, contiguous, for the palette.
    fn palette() -> Arc<[Command]> {
        [
            ("/new", "start a new session", "ctrl+x n", "session"),
            ("/sessions", "open another session", "ctrl+x l", "session"),
            (
                "/model",
                "pick the model and the provider",
                "ctrl+x m",
                "model",
            ),
        ]
        .into_iter()
        .map(|(name, words, key, group)| Command {
            name,
            words,
            key,
            group,
        })
        .collect::<Vec<_>>()
        .into()
    }

    /// The text of one drawn row, trailing blanks trimmed.
    fn text(screen: &Screen, y: u16) -> String {
        let mut out = String::new();
        for x in 0..COLUMNS {
            out.push(screen.back().cell(x, y).char().unwrap_or(' '));
        }
        out.trim_end_matches(' ').to_string()
    }

    /// One cell of the drawn screen.
    fn cell(screen: &Screen, x: u16, y: u16) -> Cell {
        screen.back().cell(x, y)
    }

    #[test]
    fn the_slash_list_draws_its_rows() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let mut list = List::commands(commands(), false);
        list.set_query("/m");
        assert_eq!(list.hits(), 3, "/model, /mode and /compact");
        assert_eq!(draw_list(&mut screen, &list, 0, COLUMNS, &inks), 5);

        assert_eq!(text(&screen, 0), "", "a blank top row on the fill");
        assert!(
            text(&screen, 1).starts_with("\u{203a} /model      pick the model and the provider"),
            "the picked row opens with the marker: {:?}",
            text(&screen, 1)
        );
        assert!(text(&screen, 1).ends_with("ctrl+x m"));
        assert_eq!(
            text(&screen, 4),
            "  \u{2191}\u{2193} choose \u{b7} tab completes \u{b7} enter runs \u{b7} esc closes"
        );
    }

    #[test]
    fn the_picked_row_is_marked_and_filled_across() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let list = List::commands(commands(), false);
        draw_list(&mut screen, &list, 0, COLUMNS, &inks);

        assert_eq!(cell(&screen, 0, 1).char(), Some('\u{203a}'));
        assert_eq!(
            cell(&screen, 0, 1).style(),
            inks.picked.mark,
            "the marker is the mark ink"
        );
        assert_eq!(
            cell(&screen, 79, 1).style(),
            inks.picked.fill,
            "the fill runs to the edge"
        );
        assert_eq!(cell(&screen, 1, 1).style(), inks.picked.fill);
        assert_eq!(
            cell(&screen, 2, 1).style(),
            inks.picked.bold,
            "the picked name is bold"
        );

        assert_eq!(cell(&screen, 0, 2).char(), Some(' '), "no marker below it");
        assert_eq!(cell(&screen, 0, 2).style(), inks.plain.fill);
        assert_eq!(cell(&screen, 2, 2).style(), inks.plain.text);
    }

    #[test]
    fn the_matched_chars_of_a_command_carry_the_mark_ink() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let mut list = List::commands(commands(), false);
        list.set_query("/m");
        draw_list(&mut screen, &list, 0, COLUMNS, &inks);

        // `/compact` is the third hit: its `/` and its `m` matched.
        let row = 3;
        assert!(text(&screen, row).starts_with("  /compact"));
        assert_eq!(cell(&screen, 2, row).char(), Some('/'));
        assert_eq!(cell(&screen, 2, row).style(), inks.plain.mark);
        assert_eq!(
            cell(&screen, 3, row).style(),
            inks.plain.text,
            "the `c` is not matched"
        );
        assert_eq!(cell(&screen, 2 + 3, row).char(), Some('m'));
        assert_eq!(cell(&screen, 2 + 3, row).style(), inks.plain.mark);
        assert_eq!(
            cell(&screen, 2, row + 2),
            Cell::BLANK,
            "nothing draws below"
        );
    }

    #[test]
    fn the_key_ends_one_column_before_the_right_edge() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let list = List::commands(commands(), false);
        draw_list(&mut screen, &list, 0, COLUMNS, &inks);

        // `/compact` has no key and draws nothing at the edge.
        assert_eq!(cell(&screen, COLUMNS - 1, 1).style(), inks.picked.fill);
        let key = cell(&screen, COLUMNS - 2, 2);
        assert_eq!(key.char(), Some('m'), "the last char of `shift+tab`");
        assert_eq!(key.style(), inks.plain.dim);
        assert_eq!(cell(&screen, COLUMNS - 1, 2).char(), Some(' '));
        assert_eq!(cell(&screen, COLUMNS - 1, 2).style(), inks.plain.fill);
    }

    #[test]
    fn nothing_is_drawn_at_or_past_the_width() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let list = List::leader();
        let width = 40;
        let rows = draw_list(&mut screen, &list, 2, width, &inks);
        for y in 2..2 + rows {
            for x in width..COLUMNS {
                assert_eq!(
                    cell(&screen, x, y),
                    Cell::BLANK,
                    "({x}, {y}) is past the width"
                );
            }
        }
    }

    #[test]
    fn the_palette_draws_its_query_row_and_its_groups() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let list = List::commands(palette(), true);
        assert_eq!(draw_list(&mut screen, &list, 0, COLUMNS, &inks), 8);

        assert_eq!(text(&screen, 1), "\u{203a} type to filter 3 commands");
        assert_eq!(cell(&screen, 2, 1).style(), inks.plain.dim);
        assert_eq!(text(&screen, 2), "  session");
        assert_eq!(cell(&screen, 2, 2).style(), inks.plain.dim);
        assert!(
            text(&screen, 3).starts_with("\u{203a} /new"),
            "{:?}",
            text(&screen, 3)
        );
        assert_eq!(text(&screen, 5), "  model");
        assert_eq!(
            text(&screen, 7),
            "  ctrl+x / closes \u{b7} your draft stays in the box"
        );
    }

    #[test]
    fn the_palette_query_row_holds_the_typed_query() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let mut list = List::commands(palette(), true);
        list.push('n');
        assert_eq!(draw_list(&mut screen, &list, 0, COLUMNS, &inks), 5);
        assert_eq!(text(&screen, 1), "\u{203a} n");
        assert_eq!(
            cell(&screen, 2, 1).style(),
            inks.plain.text,
            "the query is body text"
        );
        assert!(
            text(&screen, 2).starts_with("\u{203a} /new"),
            "no group rows under a query: {:?}",
            text(&screen, 2)
        );
    }

    #[test]
    fn the_file_list_marks_its_paths_and_counts_them() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let paths: Arc<[Arc<str>]> = ["crates/tui/src/app/list.rs", "crates/tui/src/pick.rs"]
            .into_iter()
            .map(Arc::from)
            .collect::<Vec<Arc<str>>>()
            .into();
        let mut list = List::files(paths);
        list.set_query("src");
        assert_eq!(list.hits(), 2, "both paths hold `src`");
        assert_eq!(draw_list(&mut screen, &list, 0, COLUMNS, &inks), 4);

        assert_eq!(text(&screen, 0), "");
        assert_eq!(text(&screen, 1), "\u{203a} crates/tui/src/app/list.rs");
        assert_eq!(
            cell(&screen, 2, 1).style(),
            inks.picked.bold,
            "the picked path is bold"
        );
        assert_eq!(
            cell(&screen, 13, 1).style(),
            inks.picked.mark,
            "the matched `s`"
        );
        assert_eq!(text(&screen, 2), "  crates/tui/src/pick.rs");
        assert_eq!(cell(&screen, 2, 2).style(), inks.plain.text);
        assert_eq!(
            cell(&screen, 12, 2).style(),
            inks.plain.text,
            "the `/` does not match"
        );
        assert_eq!(cell(&screen, 13, 2).style(), inks.plain.mark);
        assert_eq!(
            text(&screen, 3),
            "  2 of 2 files \u{b7} tab inserts the path \u{b7} esc closes"
        );
    }

    #[test]
    fn an_empty_file_list_draws_its_top_row_and_its_count() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let list = List::files(Vec::new().into());
        assert_eq!(draw_list(&mut screen, &list, 0, COLUMNS, &inks), 2);
        assert_eq!(text(&screen, 0), "");
        assert_eq!(
            text(&screen, 1),
            "  0 of 0 files \u{b7} tab inserts the path \u{b7} esc closes"
        );
    }

    #[test]
    fn the_leader_list_draws_its_title_grid_and_footer() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let list = List::leader();
        assert_eq!(draw_list(&mut screen, &list, 0, COLUMNS, &inks), 5);

        assert_eq!(text(&screen, 1), "ctrl+x then");
        assert_eq!(cell(&screen, 0, 1).style(), inks.plain.mark);
        assert_eq!(
            cell(&screen, 7, 1).style(),
            inks.plain.dim,
            "`then` follows `ctrl+x`"
        );

        let mut want = String::from(" ");
        for (keys, words) in [
            ("b", "sidebar"),
            ("l", "sessions"),
            ("m", "model"),
            ("tab", "sidebar focus"),
        ] {
            want.push_str(&format!("{keys:>6} {words:<16}"));
        }
        let want: String = want.chars().take(COLUMNS as usize).collect();
        assert_eq!(text(&screen, 2), want.trim_end());
        assert_eq!(
            cell(&screen, 6, 2).style(),
            inks.plain.mark,
            "the key is the mark ink"
        );
        assert_eq!(
            cell(&screen, 8, 2).style(),
            inks.plain.text,
            "its words are body text"
        );

        assert_eq!(
            text(&screen, 3),
            format!("{:>7} {:<16}", "/", "commands").trim_end()
        );

        assert_eq!(text(&screen, 4), "  esc cancels \u{b7} ? shows every key");
    }

    /// Every cell of every row of the list carries the fill of that row: the
    /// list's own fill, and the picked fill on the selected row, under the
    /// marker, the query, the groups, the names, the keys and the footer.
    #[test]
    fn every_cell_of_a_list_row_has_the_fill() {
        let mut screen = screen();
        let inks = inks(&mut screen);
        let mut filtered = List::commands(commands(), false);
        filtered.set_query("/m");
        for (list, picked) in [(List::commands(palette(), true), 3), (filtered, 1)] {
            let rows = draw_list(&mut screen, &list, 0, COLUMNS, &inks);
            for y in 0..rows {
                let want = if y == picked { PICKED } else { FILL };
                for x in 0..COLUMNS {
                    let bg = screen.style_of(cell(&screen, x, y).style()).bg;
                    assert_eq!(bg, want, "the cell at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn the_digits_of_a_count_are_written_without_allocating() {
        let mut buf = [0u8; super::DIGITS];
        assert_eq!(super::digits_of(0, &mut buf), "0");
        assert_eq!(super::digits_of(7, &mut buf), "7");
        assert_eq!(super::digits_of(1204, &mut buf), "1204");
    }
}
