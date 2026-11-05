//! Drawing a sheet in place of the text box.
//!
//! The sheet is laid out top down: a blank, the title row, the filter row on
//! its own fill, a blank, the rows, blank rows to fill the height, the footer
//! row and a last blank. Every row carries the input fill across the width and
//! the accent bar the text box draws in column 0, so the sheet reads as the box
//! it replaced.
//!
//! Three layouts share those rows: the plain list, the help sheet's groups side
//! by side in lanes of [`LANE`] columns, and the sessions sheet's rows in the
//! left half with a preview right of a rule. The width decides the last two:
//! the lanes need [`LANES`] columns and the preview needs [`SPLIT`].
//!
//! The draw builds no string and allocates nothing: every text it writes is
//! borrowed from the sheet, and each write stops at the columns the row has.
//! Every ink a row writes with carries that row's own background, so the fill
//! shows between a name and its detail instead of the terminal's own.
//!
//! A sheet with details lines them up: the name column is as wide as the
//! longest item name in the window, at most half the width, and the detail
//! starts two columns after it.

use unicode_width::UnicodeWidthStr;

use crate::screen::{Cell, Screen};

use super::super::sheet::{Kind, Mark, Sheet};

/// The rows a sheet draws around its rows: the blank above the title, the title,
/// the filter, the blank under it, the footer and the blank under that.
const CHROME: u16 = 6;

/// The accent bar of the text box, in column 0 of every row.
const BAR: &str = "\u{258e}";

/// The column a row's text starts at, after the bar.
const PAD: u16 = 1;

/// The columns of the arrow of the filter row, and of the text after it.
const ARROW: &str = "\u{203a}";
const FILTER: u16 = PAD + 2;

/// The columns an item row draws its `›` in and its name at.
const PICK_AT: u16 = PAD;
const MARK_AT: u16 = PAD + 2;
const NAME_AT: u16 = PAD + 4;

/// The column of a group or a note row's name, two columns in.
const HEAD_AT: u16 = PAD + 2;

/// The mark of the selected row.
const PICK: &str = "\u{203a}";

/// The marks of the rows in use and of the rows that wait.
const CURRENT: &str = "\u{25cf}";
const WAITS: &str = "\u{23f8}";

/// The columns the rows take when the preview shows.
const HALF: u16 = 46;

/// The columns the rule of the preview sits in, and the column its text starts
/// at after the rule's pad.
const RULE: u16 = HALF;
const PREVIEW: u16 = RULE + 2;

/// The rule between the rows and the preview.
const PREVIEW_RULE: &str = "\u{2502}";

/// The columns a sheet needs before it splits the rows and the preview.
const SPLIT: u16 = 100;

/// The columns one group of the help sheet takes.
const LANE: u16 = 32;

/// The columns a sheet needs before its groups flow side by side.
const LANES: u16 = 96;

/// The styles one row of a sheet draws with, all over the background of the
/// fill it is on.
pub(crate) struct RowInks {
    /// The blank cells of the row.
    pub fill: u32,
    /// The accent bar of column 0.
    pub bar: u32,
    /// A row's name and what the filter holds.
    pub text: u32,
    /// The name of the selected row, and the header of a group in a lane.
    pub bold: u32,
    /// Counts, hints, details, group rows and the footer.
    pub dim: u32,
    /// The title, the `●` of the current row and the preview's own text.
    pub accent: u32,
    /// The `›` of the selected row.
    pub mark: u32,
    /// The `⏸` of a row that waits.
    pub warn: u32,
    /// The rule between the rows and the preview.
    pub line: u32,
}

/// The styles one sheet draws with: a set per fill a row can carry.
pub(crate) struct SheetInks {
    /// The rows on the input fill.
    pub plain: RowInks,
    /// The selected row, on its own fill.
    pub picked: RowInks,
    /// The filter row, on its own fill.
    pub query: RowInks,
}

/// Draws `sheet` from row `y`, `rows` tall and `width` wide from column 0, and
/// answers the terminal cursor's place: the filter's edit point.
pub(crate) fn draw_sheet(
    screen: &mut Screen,
    sheet: &Sheet,
    y: u16,
    rows: u16,
    width: u16,
    inks: &SheetInks,
) -> (u16, u16) {
    let width = width.min(screen.columns());
    let plain = &inks.plain;
    let last = y.saturating_add(rows).min(screen.rows());
    // Every row of the sheet carries the fill and the bar first; the rows that
    // own another fill or another color overpaint their own cells.
    for at in y.min(last)..last {
        fill_row(screen, at, width, plain);
    }
    let title_row = y + 1;
    let filter_row = y + 2;
    let first = y + 4;
    let footer_row = last.saturating_sub(2);
    // A sheet too short for its own header draws its fill and nothing else.
    if rows < CHROME || last <= filter_row {
        return (FILTER.min(width.saturating_sub(1)), filter_row);
    }

    // The title row: the title in the accent, the count and then the hint at
    // the right edge.
    let hint_at = right_start(sheet.hint, width);
    let mut x = screen.put_str_until(PAD, title_row, sheet.title, plain.accent, hint_at);
    x = screen.put_str_until(x, title_row, "  ", plain.dim, hint_at);
    screen.put_str_until(x, title_row, &sheet.count, plain.dim, hint_at);
    screen.put_str_until(hint_at, title_row, sheet.hint, plain.dim, width);

    // The filter row, on its own fill: the arrow, then what the filter holds.
    let query = &inks.query;
    fill_row(screen, filter_row, width, query);
    let mut x = screen.put_str_until(PAD, filter_row, ARROW, query.mark, width);
    x = screen.put_str_until(x, filter_row, " ", query.text, width);
    let cursor = screen.put_str_until(x, filter_row, &sheet.filter, query.text, width);

    let slots = footer_row.saturating_sub(first) as usize;
    if sheet.columns && sheet.filter.is_empty() && width >= LANES {
        draw_lanes(screen, sheet, first, slots, width, inks);
    } else {
        draw_rows(screen, sheet, first, slots, width, inks);
    }

    // The footer: two columns in, the keys the sheet answers to.
    screen.put_str_until(PAD + 1, footer_row, sheet.footer, plain.dim, width);

    (cursor.min(width.saturating_sub(1)), filter_row)
}

/// Draws the rows of the plain sheet and the rows of the sessions sheet with
/// the preview beside them.
fn draw_rows(
    screen: &mut Screen,
    sheet: &Sheet,
    first: u16,
    slots: usize,
    width: u16,
    inks: &SheetInks,
) {
    if slots == 0 {
        return;
    }
    let split = sheet.previews && width >= SPLIT;
    // The column a row's own text stops at: the rule, when the preview shows.
    let left = if split { RULE } else { width };
    // The scroll the last frame kept, moved the least it can so the selection
    // is a row of the window and the window stays inside the rows.
    let mut top = sheet.top.get();
    if sheet.selected < top {
        top = sheet.selected;
    }
    if top + slots <= sheet.selected {
        top = sheet.selected + 1 - slots;
    }
    top = top.min(sheet.shown.len().saturating_sub(slots));
    sheet.top.set(top);
    // The column the details of the items line up at: two spaces after the
    // longest name in the window.
    let detail_at = NAME_AT + name_column(sheet, top, slots, width) + 2;

    for slot in 0..slots {
        let at = first + slot as u16;
        let index = sheet.shown.get(top + slot).copied();
        let picked = index.is_some_and(|index| {
            let row = &sheet.rows[index as usize];
            row.kind == Kind::Item && top + slot == sheet.selected
        });
        let inks = if picked { &inks.picked } else { &inks.plain };
        if picked {
            fill_row(screen, at, width, inks);
        }
        if split {
            screen.put_str_until(RULE, at, PREVIEW_RULE, inks.line, width);
            if let Some(line) = sheet.preview.get(top + slot) {
                let ink = if line.dim { inks.dim } else { inks.text };
                screen.put_str_until(PREVIEW, at, &line.text, ink, width);
            }
        }
        let Some(index) = index else {
            continue;
        };
        let row = &sheet.rows[index as usize];
        match row.kind {
            Kind::Item => {
                if picked {
                    screen.put_str_until(PICK_AT, at, PICK, inks.mark, left);
                }
                match row.mark {
                    Mark::Current => {
                        screen.put_str_until(MARK_AT, at, CURRENT, inks.accent, left);
                    }
                    Mark::Waits => {
                        screen.put_str_until(MARK_AT, at, WAITS, inks.warn, left);
                    }
                    Mark::None => {}
                }
                let ink = if picked {
                    inks.bold
                } else if row.dim {
                    inks.dim
                } else {
                    inks.text
                };
                let mut x = screen.put_str_until(NAME_AT, at, &row.name, ink, left);
                if !row.detail.is_empty() {
                    x = screen.put_str_until(detail_at.max(x), at, &row.detail, inks.dim, left);
                }
                if !row.right.is_empty() {
                    let start = right_start(&row.right, left);
                    if start > x {
                        screen.put_str_until(start, at, &row.right, inks.dim, left);
                    }
                }
            }
            Kind::Group | Kind::Note => {
                let x = screen.put_str_until(HEAD_AT, at, &row.name, inks.dim, left);
                detail(screen, x, at, row.detail.as_ref(), left, inks);
            }
            Kind::Blank => {}
        }
    }
}

/// Draws the groups of the help sheet side by side: each group's name heads a
/// lane of [`LANE`] columns and its items fill the rows under it.
fn draw_lanes(
    screen: &mut Screen,
    sheet: &Sheet,
    first: u16,
    slots: usize,
    width: u16,
    inks: &SheetInks,
) {
    if slots == 0 {
        return;
    }
    let lanes = (width / LANE) as usize;
    let plain = &inks.plain;
    let selected = sheet.shown.get(sheet.selected).copied();
    let mut index = 0usize;
    let mut lane = 0usize;
    while index < sheet.rows.len() && lane < lanes {
        if sheet.rows[index].kind != Kind::Group {
            index += 1;
            continue;
        }
        let x = lane as u16 * LANE;
        let limit = x + LANE;
        screen.put_str_until(
            x + HEAD_AT,
            first,
            &sheet.rows[index].name,
            plain.bold,
            limit,
        );
        index += 1;
        let mut line = 1usize;
        while index < sheet.rows.len() && sheet.rows[index].kind != Kind::Group {
            let row = &sheet.rows[index];
            if row.kind == Kind::Item && line < slots {
                let at = first + line as u16;
                let picked = selected == Some(index as u32);
                if picked {
                    screen.put_str_until(x + PICK_AT, at, PICK, plain.mark, limit);
                }
                let ink = if picked {
                    plain.bold
                } else if row.dim {
                    plain.dim
                } else {
                    plain.text
                };
                let end = screen.put_str_until(x + NAME_AT, at, &row.name, ink, limit);
                detail(screen, end, at, row.detail.as_ref(), limit, plain);
                line += 1;
            }
            index += 1;
        }
        lane += 1;
    }
}

/// Draws the dim detail of a row two columns after its name, and answers the
/// column after it.
fn detail(screen: &mut Screen, x: u16, at: u16, text: &str, limit: u16, inks: &RowInks) -> u16 {
    if text.is_empty() {
        return x;
    }
    let x = screen.put_str_until(x, at, "  ", inks.dim, limit);
    screen.put_str_until(x, at, text, inks.dim, limit)
}

/// The columns of the name column: the longest item name in the window, at most
/// half the sheet's width.
fn name_column(sheet: &Sheet, top: usize, slots: usize, width: u16) -> u16 {
    let mut widest = 0u16;
    for slot in 0..slots {
        let Some(&index) = sheet.shown.get(top + slot) else {
            break;
        };
        let row = &sheet.rows[index as usize];
        if row.kind == Kind::Item {
            widest = widest.max(UnicodeWidthStr::width(&*row.name) as u16);
        }
    }
    widest.min(width / 2)
}

/// Paints one row of the sheet: the fill across the width and the bar in column
/// 0, which the text box draws the same way.
fn fill_row(screen: &mut Screen, at: u16, width: u16, inks: &RowInks) {
    for x in 0..width {
        screen.set(x, at, Cell::from_char(' ', inks.fill));
    }
    screen.put_str(0, at, BAR, inks.bar);
}

/// The column a right-aligned `text` starts at, one column short of the width
/// so the row keeps a margin at its right edge.
fn right_start(text: &str, width: u16) -> u16 {
    width
        .saturating_sub(1)
        .saturating_sub(UnicodeWidthStr::width(text) as u16)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::screen::{Color, Screen, Style};

    use super::super::super::sheet::{Kind, Line, Mark, Row, Sheet};
    use super::{RowInks, SheetInks, draw_sheet};

    /// The columns and the rows of the test screen.
    const COLUMNS: u16 = 100;
    const ROWS: u16 = 40;

    /// The rows the tests draw a sheet in.
    const SHEET_ROWS: u16 = 20;

    /// The first body row, the filter row and the footer row of a sheet drawn
    /// from row 0, and the column the filter's text starts at.
    const BODY: u16 = 4;
    const FILTER: u16 = 2;
    const FOOTER: u16 = 18;
    const TEXT: u16 = 3;

    /// The column the rule of the preview sits in and the one its text starts
    /// at, in a screen wide enough for the split.
    const RULE: u16 = 46;
    const PREVIEW: u16 = 48;

    /// The fills of the tests: the rows' own, the picked row's and the filter
    /// row's.
    const FILL: Color = Color::Rgb(0x10, 0x10, 0x18);
    const PICKED: Color = Color::Rgb(0x30, 0x20, 0x10);
    const QUERY: Color = Color::Rgb(0x08, 0x08, 0x0c);

    /// The inks of one fill: a distinct foreground per role over it, and the
    /// fill itself for the blank cells.
    fn row_inks(screen: &mut Screen, bg: Color, base: u8) -> RowInks {
        fn ink(screen: &mut Screen, bg: Color, fg: u8) -> u32 {
            screen.intern(Style {
                fg: Color::Indexed(fg),
                bg,
                ..Style::default()
            })
        }
        RowInks {
            fill: screen.intern(Style {
                bg,
                ..Style::default()
            }),
            bar: ink(screen, bg, base + 1),
            text: ink(screen, bg, base + 2),
            bold: ink(screen, bg, base + 3),
            dim: ink(screen, bg, base + 4),
            accent: ink(screen, bg, base + 5),
            mark: ink(screen, bg, base + 6),
            warn: ink(screen, bg, base + 7),
            line: ink(screen, bg, base + 8),
        }
    }

    /// An ink per role and per fill, so a test names the style of a cell.
    fn inks(screen: &mut Screen) -> SheetInks {
        SheetInks {
            plain: row_inks(screen, FILL, 1),
            picked: row_inks(screen, PICKED, 20),
            query: row_inks(screen, QUERY, 40),
        }
    }

    /// A row of the tests.
    fn row(kind: Kind, name: &str, detail: &str, right: &str) -> Row {
        Row {
            kind,
            mark: Mark::None,
            name: Arc::from(name),
            detail: Arc::from(detail),
            right: Arc::from(right),
            dim: false,
        }
    }

    /// The model sheet of the tests: a group, the current row, an item, a blank
    /// row, another group and a note under it.
    fn sheet() -> Sheet {
        Sheet::new(
            "Model",
            Arc::from("38 models \u{b7} 6 providers"),
            "provider \u{b7} context",
            "\u{2191}\u{2193} choose \u{b7} enter switches",
            vec![
                row(Kind::Group, "recent", "", ""),
                Row {
                    mark: Mark::Current,
                    ..row(Kind::Item, "deepseek-flash", "200k", "now")
                },
                row(Kind::Item, "claude-sonnet", "1M", ""),
                row(Kind::Blank, "", "", ""),
                row(Kind::Group, "anthropic", "\u{b7} logged in", ""),
                row(Kind::Note, "\u{2026} 4 more providers", "", ""),
            ],
        )
    }

    /// The sessions sheet of the tests: two rows, one of them waiting.
    fn previews() -> Sheet {
        Sheet::new(
            "Sessions",
            Arc::from("6 in this folder \u{b7} 1 waits"),
            "enter opens \u{b7} ctrl+f forks",
            "\u{2191}\u{2193} choose \u{b7} enter opens",
            vec![
                row(Kind::Item, "fix the start test", "now", ""),
                Row {
                    mark: Mark::Waits,
                    ..row(Kind::Item, "steer headless", "1d", "")
                },
            ],
        )
        .with_previews()
    }

    /// The help sheet of the tests: three groups of keys, side by side.
    fn help() -> Sheet {
        Sheet::new(
            "Keys",
            Arc::from("all keys can change in keys.toml"),
            "type to filter",
            "esc closes \u{b7} ctrl+x / runs any command",
            vec![
                row(Kind::Group, "text box", "", ""),
                row(Kind::Item, "enter", "send or steer", ""),
                row(Kind::Item, "alt+enter", "queue", ""),
                row(Kind::Blank, "", "", ""),
                row(Kind::Group, "transcript", "", ""),
                row(Kind::Item, "tab", "fold the block", ""),
                row(Kind::Blank, "", "", ""),
                row(Kind::Group, "ctrl+x then", "", ""),
                row(Kind::Item, "b", "sidebar", ""),
            ],
        )
        .with_columns()
    }

    /// A screen of the given columns with the sheet's inks interned into it.
    fn screen(columns: u16) -> (Screen, SheetInks) {
        let mut screen = Screen::new(columns, ROWS);
        let inks = inks(&mut screen);
        (screen, inks)
    }

    /// The text of one row of the frame being drawn, trailing blanks trimmed.
    fn text(screen: &Screen, y: u16) -> String {
        let grid = screen.back();
        let mut row = String::new();
        for x in 0..grid.width() {
            match grid.cell(x, y).char() {
                Some(c) => row.push(c),
                None => row.push(' '),
            }
        }
        row.truncate(row.trim_end_matches(' ').len());
        row
    }

    /// Every row of the frame being drawn, top down.
    fn rows(screen: &Screen) -> Vec<String> {
        (0..screen.rows()).map(|y| text(screen, y)).collect()
    }

    /// The style id of one cell of the frame being drawn.
    fn style(screen: &Screen, x: u16, y: u16) -> u32 {
        screen.back().cell(x, y).style()
    }

    /// The column `needle` starts at in `row`, counted in cells.
    fn column(row: &str, needle: &str) -> usize {
        let byte = row.find(needle).unwrap_or_else(|| panic!("{needle:?}"));
        row[..byte].chars().count()
    }

    /// The title row holds the title in the accent, the count in dim and the
    /// hint at the right edge.
    #[test]
    fn the_title_row_holds_the_title_the_count_and_the_hint() {
        let (mut screen, inks) = screen(COLUMNS);
        let sheet = sheet();
        let (_, cursor_row) = draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!(cursor_row, FILTER);
        let title = text(&screen, 1);
        assert!(
            title.starts_with("\u{258e}Model  38 models \u{b7} 6 providers"),
            "{title}"
        );
        // The hint ends one column before the right edge.
        assert!(title.ends_with("provider \u{b7} context"), "{title}");
        assert!(title.chars().count() < COLUMNS as usize);
        assert_eq!(style(&screen, 1, 1), inks.plain.accent);
        assert_eq!(style(&screen, 7, 1), inks.plain.dim);
        assert_eq!(style(&screen, COLUMNS - 2, 1), inks.plain.dim);
        // The blank rows above and below the header carry the bar and the fill.
        assert_eq!(text(&screen, 0), "\u{258e}");
        assert_eq!(style(&screen, 0, 0), inks.plain.bar);
        assert_eq!(style(&screen, 40, 0), inks.plain.fill);
    }

    /// The filter row is the arrow and then what was typed, on its own fill.
    #[test]
    fn the_filter_row_holds_the_arrow_and_the_filter() {
        let (mut screen, inks) = screen(COLUMNS);
        let mut sheet = sheet();
        for c in "claude".chars() {
            sheet.push(c);
        }
        let (cursor, cursor_row) = draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!(text(&screen, FILTER), "\u{258e}\u{203a} claude");
        assert_eq!(style(&screen, 1, FILTER), inks.query.mark);
        assert_eq!(style(&screen, TEXT, FILTER), inks.query.text);
        assert_eq!(style(&screen, 40, FILTER), inks.query.fill);
        assert_eq!(cursor_row, FILTER);
        assert_eq!(cursor, TEXT + 6, "the edit point follows the text");
    }

    /// The footer row carries the keys of the sheet in dim, two columns in.
    #[test]
    fn the_footer_row_holds_the_keys_of_the_sheet() {
        let (mut screen, inks) = screen(COLUMNS);
        let sheet = sheet();
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!(
            text(&screen, FOOTER),
            "\u{258e} \u{2191}\u{2193} choose \u{b7} enter switches"
        );
        assert_eq!(style(&screen, 2, FOOTER), inks.plain.dim);
        // The last row of the sheet is blank, and so are the rows between the
        // last row of the list and the footer.
        assert_eq!(text(&screen, SHEET_ROWS - 1), "\u{258e}");
        assert_eq!(text(&screen, BODY + 7), "\u{258e}");
    }

    /// The selected row carries the `›` in the accent and the picked fill across
    /// the width, and the row under it carries neither.
    #[test]
    fn the_selected_row_draws_its_marker_and_its_fill() {
        let (mut screen, inks) = screen(COLUMNS);
        let sheet = sheet();
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        // The first row of the list is a group, so the pick is one row down.
        let picked = BODY + 1;
        let row = text(&screen, picked);
        assert!(
            row.starts_with("\u{258e}\u{203a} \u{25cf} deepseek-flash  200k"),
            "{row}"
        );
        assert!(row.ends_with("now"), "{row}");
        assert_eq!(style(&screen, 1, picked), inks.picked.mark);
        assert_eq!(style(&screen, 3, picked), inks.picked.accent);
        assert_eq!(style(&screen, 5, picked), inks.picked.bold);
        assert_eq!(style(&screen, 40, picked), inks.picked.fill);
        assert_eq!(style(&screen, COLUMNS - 4, picked), inks.picked.dim);

        let under = BODY + 2;
        assert!(
            text(&screen, under).starts_with("\u{258e}    claude-sonnet"),
            "{}",
            text(&screen, under)
        );
        assert_eq!(style(&screen, 1, under), inks.plain.fill);
        assert_eq!(style(&screen, 40, under), inks.plain.fill);
        assert_eq!(style(&screen, 5, under), inks.plain.text);
    }

    /// The group and the note rows draw their name in dim and no marker.
    #[test]
    fn the_group_and_the_note_rows_draw_dim() {
        let (mut screen, inks) = screen(COLUMNS);
        let sheet = sheet();
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!(text(&screen, BODY), "\u{258e}  recent");
        assert_eq!(style(&screen, 3, BODY), inks.plain.dim);
        assert_eq!(
            text(&screen, BODY + 4),
            "\u{258e}  anthropic  \u{b7} logged in"
        );
        assert_eq!(
            text(&screen, BODY + 5),
            "\u{258e}  \u{2026} 4 more providers"
        );
        assert_eq!(style(&screen, 3, BODY + 5), inks.plain.dim);
    }

    /// The background of one cell of the frame being drawn.
    fn bg(screen: &Screen, x: u16, y: u16) -> Color {
        screen.style_of(style(screen, x, y)).bg
    }

    /// Every cell of every row of a sheet carries the fill of that row: the
    /// input fill, the picked fill on the selected row and the filter fill on
    /// the filter row, under the text, the gaps, the details and the preview.
    #[test]
    fn every_cell_of_a_sheet_row_has_the_fill() {
        let (mut screen, inks) = screen(COLUMNS);
        for (mut sheet, picked) in [(sheet(), BODY + 1), (previews(), BODY)] {
            sheet.set_preview(vec![
                Line {
                    text: Arc::from("fix the start test  deepseek-flash \u{b7} 1d"),
                    dim: false,
                },
                Line {
                    text: Arc::from("\u{23f8} waits for an approval: bash"),
                    dim: true,
                },
            ]);
            draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
            for y in 0..SHEET_ROWS {
                let want = if y == FILTER {
                    QUERY
                } else if y == picked {
                    PICKED
                } else {
                    FILL
                };
                for x in 0..COLUMNS {
                    assert_eq!(bg(&screen, x, y), want, "the cell at {x},{y}");
                }
            }
        }
    }

    /// Three items with names of different lengths start their details in one
    /// column, two spaces after the longest of the names.
    #[test]
    fn sheet_details_line_up() {
        let (mut screen, inks) = screen(COLUMNS);
        let sheet = Sheet::new(
            "Model",
            Arc::from("3 models"),
            "provider \u{b7} context",
            "esc closes",
            vec![
                row(Kind::Item, "b", "one", ""),
                row(Kind::Item, "deepseek-flash", "two", ""),
                row(Kind::Item, "claude-sonnet-5-5", "three", ""),
            ],
        );
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        let want = 5 + "claude-sonnet-5-5".len() + 2;
        for (offset, detail) in ["one", "two", "three"].into_iter().enumerate() {
            let line = text(&screen, BODY + offset as u16);
            assert_eq!(column(&line, detail), want, "{line}");
        }
        // A name longer than half the width stops the column there.
        let mut sheet = sheet;
        sheet.rows[2].name = Arc::from("a".repeat(80));
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        let line = text(&screen, BODY);
        assert_eq!(column(&line, "one"), 5 + 50 + 2, "{line}");
    }

    /// The marks of the rows draw in their own inks, and a waiting row says so.
    #[test]
    fn the_mark_of_a_row_draws_in_its_own_ink() {
        let (mut screen, inks) = screen(COLUMNS);
        let sheet = previews();
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!(style(&screen, 1, BODY), inks.picked.mark);
        assert!(
            text(&screen, BODY).contains("\u{203a}   fix the start test"),
            "{}",
            text(&screen, BODY)
        );
        assert_eq!(style(&screen, 3, BODY + 1), inks.plain.warn);
        assert!(
            text(&screen, BODY + 1).contains("\u{23f8} steer headless"),
            "{}",
            text(&screen, BODY + 1)
        );
    }

    /// At a hundred columns the rows take the left half and the preview lines
    /// sit right of a rule, in the ink each of them names.
    #[test]
    fn the_preview_splits_the_rows_at_a_hundred_columns() {
        let (mut screen, inks) = screen(COLUMNS);
        let mut sheet = previews();
        sheet.set_preview(vec![
            Line {
                text: Arc::from("steer headless  deepseek-flash \u{b7} 1d"),
                dim: false,
            },
            Line {
                text: Arc::from("\u{23f8} waits for an approval: bash"),
                dim: true,
            },
        ]);
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        let first = text(&screen, BODY);
        assert!(first.contains("fix the start test"), "{first}");
        assert_eq!(screen.back().cell(RULE, BODY).char(), Some('\u{2502}'));
        assert_eq!(style(&screen, RULE, BODY), inks.picked.line);
        assert_eq!(
            column(&first, "steer headless"),
            PREVIEW as usize,
            "{first}"
        );
        assert_eq!(style(&screen, PREVIEW, BODY), inks.picked.text);
        let second = text(&screen, BODY + 1);
        assert_eq!(
            column(&second, "\u{23f8} waits for an approval"),
            PREVIEW as usize,
            "{second}"
        );
        assert_eq!(style(&screen, PREVIEW, BODY + 1), inks.plain.dim);
    }

    /// Under a hundred columns the preview is off: no rule draws and the rows
    /// take the whole width.
    #[test]
    fn a_narrow_sheet_draws_no_preview() {
        let (mut screen, inks) = screen(80);
        let mut sheet = previews();
        sheet.set_preview(vec![Line {
            text: Arc::from("a preview line"),
            dim: false,
        }]);
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, 80, &inks);
        let drawn = rows(&screen);
        assert!(
            drawn.iter().all(|row| !row.contains('\u{2502}')),
            "{}",
            drawn.join("\n")
        );
        assert!(
            drawn.iter().all(|row| !row.contains("a preview line")),
            "{}",
            drawn.join("\n")
        );
    }

    /// At a hundred columns the groups of the help sheet flow side by side in
    /// lanes of 32 columns, each with its keys under it.
    #[test]
    fn the_help_groups_flow_side_by_side_at_a_hundred_columns() {
        let (mut screen, inks) = screen(COLUMNS);
        let sheet = help();
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        let head = text(&screen, BODY);
        let (box_at, transcript_at, keys_at) = (
            column(&head, "text box"),
            column(&head, "transcript"),
            column(&head, "ctrl+x then"),
        );
        assert!(box_at < transcript_at && transcript_at < keys_at, "{head}");
        assert_eq!(box_at, 3);
        assert_eq!(transcript_at, 35);
        assert_eq!(keys_at, 67);
        assert_eq!(style(&screen, 3, BODY), inks.plain.bold);

        let items = text(&screen, BODY + 1);
        assert_eq!(column(&items, "enter  send or steer"), 5);
        assert_eq!(column(&items, "tab  fold the block"), 5 + 32);
        assert_eq!(column(&items, "b  sidebar"), 5 + 64);
        // The pick starts on the first item, and a lane shows it in the bold.
        assert!(items.starts_with("\u{258e}\u{203a}"), "{items}");
        assert_eq!(style(&screen, 5, BODY + 1), inks.plain.bold);
        assert_eq!(style(&screen, 5 + 32, BODY + 1), inks.plain.text);
    }

    /// The answer of the draw is the filter's edit point: the row of the filter
    /// and the column after its text, and never past the width.
    #[test]
    fn the_cursor_answers_the_edit_point_of_the_filter() {
        let (mut screen, inks) = screen(COLUMNS);
        let mut sheet = sheet();
        let (cursor, cursor_row) = draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!((cursor, cursor_row), (TEXT, FILTER));

        for c in "model".chars() {
            sheet.push(c);
        }
        let (cursor, _) = draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!(cursor, TEXT + 5);

        // A filter wider than the sheet keeps the edit point inside it.
        for _ in 0..200 {
            sheet.push('m');
        }
        let (cursor, _) = draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, 20, &inks);
        assert_eq!(cursor, 19);
    }

    /// Nothing draws at or past the width the sheet was given.
    #[test]
    fn nothing_draws_at_or_past_the_width() {
        let (mut screen, inks) = screen(COLUMNS);
        let mut sheet = sheet();
        sheet.push('c');
        let width = 60;
        let (cursor, _) = draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, width, &inks);
        assert!(cursor < width);
        for y in 0..screen.rows() {
            for x in width..COLUMNS {
                let cell = screen.back().cell(x, y);
                assert_eq!(cell.char(), Some(' '), "a cell at {x},{y} was written");
                assert_eq!(cell.style(), 0, "a cell at {x},{y} was written");
            }
        }
    }

    /// The rows scroll so the selection stays on screen, and they come back
    /// with it.
    #[test]
    fn the_rows_scroll_to_keep_the_selection_visible() {
        let (mut screen, inks) = screen(COLUMNS);
        let rows: Vec<Row> = (0..30)
            .map(|index| row(Kind::Item, &format!("item {index}"), "", ""))
            .collect();
        let mut sheet = Sheet::new("Model", Arc::from("30 models"), "", "esc closes", rows);
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert_eq!(text(&screen, BODY), "\u{258e}\u{203a}   item 0");

        // The body holds the rows the sheet has room for: the last of the
        // thirty comes on screen with the pick, and the first row went off it.
        sheet.step(30);
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        let body = SHEET_ROWS - 6;
        let last = text(&screen, BODY + body - 1);
        assert!(last.contains("item 29"), "{last}");
        assert!(last.starts_with("\u{258e}\u{203a}"), "{last}");
        assert!(
            text(&screen, BODY).contains("item 16"),
            "{}",
            text(&screen, BODY)
        );

        sheet.step(-30);
        draw_sheet(&mut screen, &sheet, 0, SHEET_ROWS, COLUMNS, &inks);
        assert!(text(&screen, BODY).contains("item 0"));
    }
}
