use super::cell::{Cell, Color, GraphemeArena, Style};
use super::grid::Grid;
use super::testing::{Vt100Terminal, lines};
use base64::Engine as _;
use std::sync::Arc;

use super::{
    ALT_SCREEN_OFF, CommitBlock, GRAPHEME_ARENA_CAP, Run, Screen, Source, TRANSCRIPT_RING_LINES,
    Terminal,
};
use crate::picture_native::{Native, Protocol};

async fn render(screen: &mut Screen, term: &mut Vt100Terminal, out: &mut Vec<u8>) {
    screen.diff(out);
    term.write_frame(out).await.expect("write frame");
}

/// Every row of the terminal, scrollback first, trailing spaces trimmed.
fn all_rows(term: &Vt100Terminal, columns: u16) -> Vec<String> {
    term.rows_text(columns)
        .into_iter()
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// A complete live area of three rows.
fn draw_live(screen: &mut Screen, style: u32) {
    screen.clear();
    screen.put_str(0, 0, "LIVE0", style);
    screen.put_str(0, 1, "LIVE1", style);
    screen.put_str(0, 2, "LIVE2", style);
}

/// The `index`th distinct multi-codepoint grapheme of width one.
fn distinct_grapheme(index: u32) -> String {
    let mark = |k: u32| char::from_u32(0x0300 + k % 0x60).expect("combining mark");
    format!(
        "e{}{}{}",
        mark(index),
        mark(index / 0x60),
        mark(index / 0x2400)
    )
}

#[test]
fn cell_is_eight_bytes() {
    assert_eq!(std::mem::size_of::<Cell>(), 8);
}

#[test]
fn wide_char_takes_two_cells() {
    let mut arena = GraphemeArena::new();
    let mut grid = Grid::new(10, 1);
    let next = grid.put_str(0, 0, "世", 0, &mut arena);
    assert_eq!(next, 2);
    assert_eq!(grid.cell(0, 0).char(), Some('世'));
    assert!(grid.cell(1, 0).is_continuation());
}

#[test]
fn combining_mark_is_one_cell() {
    let mut arena = GraphemeArena::new();
    let mut grid = Grid::new(10, 1);
    let next = grid.put_str(0, 0, "e\u{301}", 0, &mut arena);
    assert_eq!(next, 1);
    assert!(grid.cell(0, 0).is_grapheme());
    assert!(!grid.cell(1, 0).is_continuation());
    assert_eq!(arena.get(grid.cell(0, 0).grapheme_index()), "e\u{301}");
}

#[test]
fn equal_frames_write_only_sync_brackets() {
    let mut screen = Screen::new(4, 2);
    let mut out = Vec::new();
    let style = screen.default_style();

    screen.clear();
    screen.put_str(0, 0, "ab", style);
    screen.put_str(0, 1, "cd", style);
    screen.diff(&mut out);

    screen.clear();
    screen.put_str(0, 0, "ab", style);
    screen.put_str(0, 1, "cd", style);
    screen.diff(&mut out);

    assert_eq!(
        &out[..],
        b"\x1b[?2026h\x1b[?2026l" as &[u8],
        "an equal frame writes only the DEC 2026 brackets"
    );
}

#[test]
fn one_char_change_writes_one_cursor_move_and_the_char() {
    let mut screen = Screen::new(6, 1);
    let mut out = Vec::new();
    let style = screen.default_style();

    screen.clear();
    screen.put_str(0, 0, "aaaaaa", style);
    screen.diff(&mut out);

    screen.clear();
    screen.put_str(0, 0, "aaabaa", style);
    screen.diff(&mut out);

    assert_eq!(
        &out[..],
        b"\x1b[?2026h\x1b[1;4Hb\x1b[?2026l" as &[u8],
        "one cursor move, no style change, one char"
    );
}

/// A blank cell over a fill goes out as a plain space, so text copied from
/// the terminal or from tmux holds spaces.
#[test]
fn filled_blank_cells_go_out_as_spaces() {
    let mut screen = Screen::new(6, 1);
    let mut out = Vec::new();
    let fill = screen.intern(Style {
        bg: Color::Rgb(24, 24, 24),
        ..Style::default()
    });

    screen.clear();
    screen.put_str(0, 0, "a b   ", fill);
    screen.diff(&mut out);

    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("a b   "), "the filled blanks: {text:?}");
    assert!(!text.contains('\u{a0}'), "no no-break space: {text:?}");
}

#[test]
fn one_char_change_with_a_style_writes_the_sgr() {
    let mut screen = Screen::new(6, 1);
    let mut out = Vec::new();
    let def = screen.default_style();
    let bold = screen.intern(Style {
        bold: true,
        ..Style::default()
    });

    screen.clear();
    screen.put_str(0, 0, "aaaaaa", def);
    screen.diff(&mut out);

    screen.clear();
    screen.put_str(0, 0, "aaa", def);
    screen.put_str(3, 0, "b", bold);
    screen.put_str(4, 0, "aa", def);
    screen.diff(&mut out);

    let text = String::from_utf8_lossy(&out);
    assert_eq!(text.matches('H').count(), 1, "one cursor move: {text:?}");
    assert!(
        text.contains("\x1b[1;4H"),
        "cursor move to the cell: {text:?}"
    );
    assert!(text.contains("\x1b[1m"), "bold on: {text:?}");
    assert!(text.ends_with("b\x1b[?2026l"), "the changed char: {text:?}");
}

#[tokio::test]
async fn vt100_matches_last_back_grid() {
    let columns = 20;
    let rows = 4;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::new(columns, rows);
    let mut out = Vec::new();
    let def = screen.default_style();
    let bold = screen.intern(Style {
        bold: true,
        ..Style::default()
    });
    let green = screen.intern(Style {
        fg: Color::Indexed(2),
        ..Style::default()
    });

    screen.clear();
    screen.put_str(0, 0, "first", def);
    render(&mut screen, &mut term, &mut out).await;

    screen.clear();
    screen.put_str(0, 0, "hello", def);
    screen.put_str(0, 1, "world", bold);
    screen.put_str(0, 2, "x", green);
    render(&mut screen, &mut term, &mut out).await;

    let vt_rows = term.rows_text(columns);
    for y in 0..screen.rows() {
        assert_eq!(
            vt_rows[y as usize].trim_end(),
            screen.row_text(y),
            "row {y} differs from the last drawn frame"
        );
    }

    let bold_cell = term.screen().cell(1, 0).expect("cell");
    assert!(bold_cell.bold());
    assert_eq!(bold_cell.contents(), "w");

    let green_cell = term.screen().cell(2, 0).expect("cell");
    assert_eq!(green_cell.fgcolor(), vt100::Color::Idx(2));
    assert_eq!(green_cell.contents(), "x");
}

#[tokio::test]
async fn inline_committed_lines_sit_above_the_live_area() {
    let columns = 20;
    let rows = 10;
    let live = 3;
    let mut term = Vt100Terminal::new(rows, columns, 32);
    let mut screen = Screen::inline(columns, rows, live);
    let mut out = Vec::new();
    let style = screen.default_style();

    screen.clear();
    screen.put_str(0, 0, "live one", style);
    screen.put_str(0, 1, "live two", style);
    screen.put_str(0, 2, "input", style);
    render(&mut screen, &mut term, &mut out).await;

    let (runs, spans) = lines(&["committed alpha", "committed beta"]);
    screen.commit(&runs, &spans);
    screen.clear();
    screen.put_str(0, 0, "live one", style);
    screen.put_str(0, 1, "live two", style);
    screen.put_str(0, 2, "input", style);
    render(&mut screen, &mut term, &mut out).await;

    let vt_rows: Vec<String> = term
        .rows_text(columns)
        .into_iter()
        .map(|row| row.trim_end().to_string())
        .collect();
    let alpha = vt_rows
        .iter()
        .position(|row| row == "committed alpha")
        .expect("first committed line is on screen");
    let beta = vt_rows
        .iter()
        .position(|row| row == "committed beta")
        .expect("second committed line is on screen");
    let live_one = vt_rows
        .iter()
        .position(|row| row == "live one")
        .expect("live area is on screen");
    assert!(alpha < beta, "committed lines keep their order");
    assert!(beta < live_one, "committed lines are above the live area");

    screen.clear();
    screen.put_str(0, 0, "live one", style);
    screen.put_str(0, 1, "live two", style);
    screen.put_str(0, 2, "input", style);
    render(&mut screen, &mut term, &mut out).await;

    let bytes = term.bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(
        text.matches("committed alpha").count(),
        1,
        "a committed line is written once and never redrawn"
    );
    assert_eq!(text.matches("committed beta").count(), 1);
}

#[tokio::test]
async fn a_frame_is_one_write_frame_call() {
    let columns = 8;
    let rows = 2;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::new(columns, rows);
    let mut out = Vec::new();
    let style = screen.default_style();

    for frame in 0..3u8 {
        screen.clear();
        screen.put_str(0, 0, "frame", style);
        let _ = screen.put_str(0, 1, if frame % 2 == 0 { "a" } else { "b" }, style);
        render(&mut screen, &mut term, &mut out).await;
    }

    assert_eq!(term.writes(), 3, "one write_frame call per frame");
}

#[test]
fn steady_state_reuses_every_buffer() {
    let columns = 8;
    let rows = 2;
    let mut screen = Screen::new(columns, rows);
    let mut out = Vec::new();
    let def = screen.default_style();
    let dim = screen.intern(Style {
        dim: true,
        ..Style::default()
    });

    let draw = |screen: &mut Screen| {
        screen.clear();
        screen.put_str(0, 0, "steady", def);
        screen.put_str(0, 1, "frames", dim);
    };

    draw(&mut screen);
    screen.diff(&mut out);

    let before = (
        out.capacity(),
        screen.back().cells_capacity(),
        screen.back().dirty_capacity(),
        screen.commit_capacity(),
        screen.style_count(),
        screen.grapheme_count(),
    );

    for _ in 0..1000 {
        draw(&mut screen);
        screen.diff(&mut out);
    }

    let after = (
        out.capacity(),
        screen.back().cells_capacity(),
        screen.back().dirty_capacity(),
        screen.commit_capacity(),
        screen.style_count(),
        screen.grapheme_count(),
    );
    assert_eq!(
        before, after,
        "steady-state frame changed a buffer capacity"
    );
}

#[test]
fn resize_recomputes_the_live_area() {
    let mut screen = Screen::inline(20, 10, 3);
    let style = screen.default_style();
    screen.clear();
    screen.put_str(0, 0, "live", style);
    screen.diff(&mut Vec::new());

    screen.resize(16, 8);
    assert_eq!(screen.columns(), 16);
    assert_eq!(screen.term_rows(), 8);
    assert_eq!(screen.rows(), 3, "the live area keeps its height");
    assert!(screen.is_dirty(), "a resize redraws the live area in full");
}

#[tokio::test]
async fn commit_writes_one_line_per_committed_line() {
    let columns = 10;
    let rows = 10;
    let live = 3;
    let sizes = [
        1,
        live as usize - 1,
        live as usize,
        live as usize + 1,
        2 * live as usize,
    ];
    for count in sizes {
        let mut term = Vt100Terminal::new(rows, columns, 64);
        let mut screen = Screen::inline(columns, rows, live);
        let mut out = Vec::new();
        let style = screen.default_style();

        draw_live(&mut screen, style);
        render(&mut screen, &mut term, &mut out).await;

        let lines: Vec<String> = (0..count).map(|i| format!("c{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (runs, spans) = super::testing::lines(&refs);
        screen.commit(&runs, &spans);
        draw_live(&mut screen, style);
        render(&mut screen, &mut term, &mut out).await;

        let screen_rows = all_rows(&term, columns);
        let first = screen_rows
            .iter()
            .position(|row| row == "c0")
            .expect("the first committed line is on screen");
        assert_eq!(
            &screen_rows[first..first + count],
            &lines[..],
            "committing {count} lines keeps them one per row: {screen_rows:?}"
        );
        assert_eq!(
            screen_rows[first + count],
            "LIVE0",
            "committing {count} lines leaves no blank row between the last line and the live area: \
             {screen_rows:?}"
        );
    }
}

#[tokio::test]
async fn a_resize_then_sync_still_draws_every_row() {
    // A terminal that still holds the frame it showed before the resize.
    let mut term = Vt100Terminal::new(4, 12, 0);
    term.write_frame(b"XXXX\r\nXXXX\r\nXXXX\r\nXXXX")
        .await
        .expect("stale frame");

    let mut screen = Screen::new(12, 4);
    let style = screen.default_style();
    let mut out = Vec::new();
    screen.clear();
    for (y, text) in ["aaaa", "bbbb", "cccc", "dddd"].iter().enumerate() {
        screen.put_str(0, y as u16, text, style);
    }
    screen.diff(&mut out);

    screen.resize(10, 4);
    screen.sync();
    screen.put_str(0, 1, "b", style);
    render(&mut screen, &mut term, &mut out).await;

    let expected: Vec<String> = ["", "b", "", ""]
        .iter()
        .map(|row| (*row).to_string())
        .collect();
    assert_eq!(
        all_rows(&term, 12),
        expected,
        "every row is drawn after a resize, not only the dirty one"
    );
}

#[tokio::test]
async fn a_commit_then_sync_still_draws_the_live_area() {
    let columns = 10;
    let rows = 10;
    let live = 3;
    let mut term = Vt100Terminal::new(rows, columns, 64);
    let mut screen = Screen::inline(columns, rows, live);
    let mut out = Vec::new();
    let style = screen.default_style();

    draw_live(&mut screen, style);
    render(&mut screen, &mut term, &mut out).await;

    let (runs, spans) = lines(&["c0"]);
    screen.commit(&runs, &spans);
    screen.sync();
    screen.put_str(0, 2, "LIVE2b", style);
    render(&mut screen, &mut term, &mut out).await;

    let screen_rows = all_rows(&term, columns);
    let last = screen_rows.len();
    assert_eq!(
        &screen_rows[last - live as usize..],
        &["LIVE0", "LIVE1", "LIVE2b"],
        "every live row is drawn after a commit, not only the dirty one: {screen_rows:?}"
    );
    assert_eq!(
        screen_rows[last - live as usize - 1],
        "c0",
        "the committed line sits right above the live area"
    );
}

#[test]
fn distinct_graphemes_stay_under_the_arena_cap() {
    let mut screen = Screen::new(1, 1);
    let mut out = Vec::new();
    let style = screen.default_style();
    let mut last = String::new();

    for index in 0..100_000 {
        last = distinct_grapheme(index);
        screen.put_str(0, 0, &last, style);
        screen.diff(&mut out);
        assert!(
            screen.grapheme_count() <= GRAPHEME_ARENA_CAP,
            "frame {index} left {} graphemes in the arena, over the cap of {GRAPHEME_ARENA_CAP}",
            screen.grapheme_count()
        );
    }

    assert_eq!(
        screen.row_text(0),
        last,
        "the last drawn grapheme still reads back after the arena was rebuilt"
    );
}

#[test]
fn a_compacted_arena_keeps_equal_graphemes_equal() {
    let mut screen = Screen::new(4, 2);
    let mut out = Vec::new();
    let style = screen.default_style();

    // Row 0 keeps one grapheme; row 1 walks distinct ones until the arena is rebuilt.
    screen.put_str(0, 0, "e\u{301}", style);
    screen.diff(&mut out);
    let mut seen = screen.grapheme_count();
    let mut compacted = false;
    for index in 0..2 * GRAPHEME_ARENA_CAP as u32 {
        let text = distinct_grapheme(index);
        screen.set(0, 1, Cell::from_char(' ', style));
        screen.put_str(0, 1, &text, style);
        screen.diff(&mut out);
        if screen.grapheme_count() < seen {
            compacted = true;
            break;
        }
        seen = screen.grapheme_count();
    }
    assert!(compacted, "the arena is rebuilt once it passes the cap");
    assert_eq!(
        screen.row_text(0),
        "e\u{301}",
        "the grapheme of row 0 survived the rebuild"
    );

    // The same grapheme drawn again after the rebuild is the same cell.
    screen.put_str(0, 0, "e\u{301}", style);
    screen.diff(&mut out);
    assert_eq!(
        &out[..],
        b"\x1b[?2026h\x1b[?2026l",
        "equal graphemes compare equal after the arena was rebuilt"
    );
}

#[test]
fn a_one_row_change_diffs_one_row() {
    let mut screen = Screen::new(200, 50);
    let mut out = Vec::new();
    let style = screen.default_style();

    screen.clear();
    for y in 0..50 {
        screen.put_str(0, y, "row", style);
    }
    screen.diff(&mut out);

    // `diff` leaves the drawn frame in both grids, so a frame redraws only the
    // rows it changes and never calls `clear`.
    screen.clear_row(17);
    screen.put_str(0, 17, "changed", style);
    screen.diff(&mut out);

    assert_eq!(
        screen.diff_rows_visited(),
        1,
        "only the changed row reaches the diff"
    );
    let text = String::from_utf8_lossy(&out);
    assert_eq!(text.matches('H').count(), 1, "one cursor move: {text:?}");
}

/// The first character of a frame that a terminal must never receive from cell
/// text: a control character outside the CSI escapes the diff writes itself and
/// the carriage return of a committed line break.
fn frame_control_leak(frame: &[u8]) -> Option<char> {
    let text = std::str::from_utf8(frame).expect("a frame is UTF-8");
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            assert_eq!(
                chars.next(),
                Some('['),
                "the frame writes only CSI escapes of its own"
            );
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
            continue;
        }
        if c == '\r' && chars.next() == Some('\n') {
            continue;
        }
        if is_control(c) {
            return Some(c);
        }
    }
    None
}

fn is_control(c: char) -> bool {
    matches!(c, '\0'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}')
}

#[tokio::test]
async fn a_cleared_row_is_repainted_on_the_fourth_frame() {
    let columns = 8;
    let rows = 4;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::new(columns, rows);
    let mut out = Vec::new();
    let style = screen.default_style();

    screen.clear();
    for y in 0..rows {
        screen.put_str(0, y, "row", style);
    }
    render(&mut screen, &mut term, &mut out).await;

    screen.clear_row(2);
    screen.put_str(0, 2, "changed", style);
    render(&mut screen, &mut term, &mut out).await;

    screen.clear_row(3);
    screen.put_str(0, 3, "other", style);
    render(&mut screen, &mut term, &mut out).await;

    // Frame 4 draws the text frame 1 had on row 2: the row the terminal still
    // shows has to go back to it.
    screen.clear_row(2);
    screen.put_str(0, 2, "row", style);
    render(&mut screen, &mut term, &mut out).await;

    assert_eq!(
        all_rows(&term, columns),
        ["row", "row", "row", "other"],
        "row 2 is repainted on the fourth frame"
    );
    for y in 0..rows {
        assert_eq!(
            screen.row_text(y),
            ["row", "row", "row", "other"][y as usize],
            "row {y} of the front grid"
        );
    }
}

#[test]
fn a_tab_is_one_space() {
    let mut arena = GraphemeArena::new();
    let mut grid = Grid::new(10, 2);

    let next = grid.put_str(0, 0, "a\tb", 0, &mut arena);
    assert_eq!(next, 3, "a tab takes one column");
    assert_eq!(grid.cell(1, 0).char(), Some(' '), "a tab draws one space");
    assert_eq!(grid.cell(2, 0).char(), Some('b'));
    assert!(!grid.cell(1, 0).is_continuation());

    let next = grid.put_str(0, 1, "é\t世", 0, &mut arena);
    assert_eq!(next, 4, "a tab in non-ASCII text takes one column");
    assert_eq!(grid.cell(1, 1).char(), Some(' '));
    assert_eq!(grid.cell(2, 1).char(), Some('世'));
}

#[test]
fn control_characters_never_reach_the_frame() {
    let texts = [
        "plain \x1b[2J bell\x07 cr\r del\x7f end",
        "\u{e9}\x1b[2J\tx",
        "é\x07\u{7f}\r\u{9b}2J",
        "世\u{85}\u{9b}31mred",
        "e\u{301}\u{1b}\u{7f}tail",
        "\u{9b}\u{85}\u{1b}[1;1H",
    ];
    for text in texts {
        let mut screen = Screen::new(24, 2);
        let mut out = Vec::new();
        let style = screen.default_style();
        screen.clear();
        screen.put_str(0, 0, text, style);
        screen.put_str(0, 1, text, style);
        screen.diff(&mut out);

        assert_eq!(
            frame_control_leak(&out),
            None,
            "the frame of {text:?} carries a control character"
        );
        for y in 0..2 {
            assert!(
                !screen.row_text(y).chars().any(is_control),
                "row {y} of {text:?} holds a control character"
            );
        }
    }
}

#[tokio::test]
async fn committed_lines_filter_control_characters_and_keep_one_row_each() {
    let columns = 32;
    let rows = 8;
    let mut term = Vt100Terminal::new(rows, columns, 16);
    let mut screen = Screen::inline(columns, rows, 2);
    let mut out = Vec::new();
    let (runs, spans) = lines(&[
        "plain \x1b[2J bell\x07 cr\r del\x7f end",
        "a\tb",
        "\u{9b}2J世\tx",
    ]);
    screen.commit(&runs, &spans);
    render(&mut screen, &mut term, &mut out).await;

    // The frame holds the app's own line breaks and its CSI escapes, and no
    // other control character from the committed text.
    let written = String::from_utf8_lossy(&out)
        .replace("\r\n", "")
        .replace('\n', "");
    assert_eq!(
        frame_control_leak(written.as_bytes()),
        None,
        "a committed line carries no control character: {written:?}"
    );
    let screen_rows = all_rows(&term, columns);
    let first = screen_rows
        .iter()
        .position(|row| row.starts_with("plain"))
        .expect("the first committed line is on screen");
    assert_eq!(
        &screen_rows[first..first + 3],
        &["plain [2J bell cr del end", "a b", "2J世 x"],
        "a committed line drops every control character, a tab takes one column: {screen_rows:?}"
    );
}

#[tokio::test]
async fn committed_lines_keep_their_markdown_styles() {
    let columns = 16;
    let rows = 6;
    let mut term = Vt100Terminal::new(rows, columns, 8);
    let mut screen = Screen::inline(columns, rows, 2);
    let mut out = Vec::new();
    let plain = Run::plain("plain ");
    let bold = Run {
        text: "bold",
        style: screen.intern(Style {
            bold: true,
            ..Style::default()
        }),
    };
    let line = 0..2;
    screen.commit(&[plain, bold], std::slice::from_ref(&line));
    render(&mut screen, &mut term, &mut out).await;

    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("\x1b[1m"),
        "the bold run keeps its style: {text:?}"
    );
    let found = term
        .screen()
        .rows(0, columns)
        .find(|row| row.contains("bold"))
        .expect("the line is on screen");
    assert!(
        found.trim_end() == "plain bold",
        "the runs of a line write in order: {found:?}"
    );
}

#[tokio::test]
async fn a_shrinking_live_area_leaves_no_band_between_the_transcript_and_the_editor() {
    let columns = 10;
    let rows = 10;
    let mut term = Vt100Terminal::new(rows, columns, 32);
    let mut screen = Screen::inline(columns, rows, 6);
    let mut out = Vec::new();
    let style = screen.default_style();

    draw_box(&mut screen, style, 6);
    render(&mut screen, &mut term, &mut out).await;

    let (runs, spans) = lines(&["c0"]);
    screen.commit(&runs, &spans);
    screen.fit(2);
    screen.clear();
    screen.put_str(0, 0, "EDIT", style);
    screen.put_str(0, 1, "status", style);
    render(&mut screen, &mut term, &mut out).await;

    let screen_rows = all_rows(&term, columns);
    let first = screen_rows
        .iter()
        .position(|row| row == "c0")
        .expect("the committed line is on screen");
    assert_eq!(
        &screen_rows[first..first + 4],
        &["c0", "EDIT", "status", ""],
        "the rows the live area gave back are erased and the editor follows the transcript: \
         {screen_rows:?}"
    );
}

#[tokio::test]
async fn a_live_area_that_took_the_whole_screen_gives_the_rows_back_from_the_top() {
    let columns = 10;
    let rows = 10;
    let mut term = Vt100Terminal::new(rows, columns, 32);
    let mut screen = Screen::inline(columns, rows, 2);
    let mut out = Vec::new();
    let style = screen.default_style();

    // The area grows past the bottom of the screen, so it takes every row the
    // transcript had.
    draw_box(&mut screen, style, 2);
    render(&mut screen, &mut term, &mut out).await;
    screen.fit(10);
    draw_box(&mut screen, style, 10);
    render(&mut screen, &mut term, &mut out).await;

    // It gives them back: there is no transcript row above it to print under, so
    // the area goes to the bottom of the screen and the lines committed next
    // start at the top, in order, instead of above the screen.
    screen.fit(2);
    screen.clear();
    screen.put_str(0, 0, "EDIT", style);
    screen.put_str(0, 1, "status", style);
    let (runs, spans) = lines(&["c0", "c1"]);
    screen.commit(&runs, &spans);
    render(&mut screen, &mut term, &mut out).await;

    assert_eq!(
        all_rows(&term, columns),
        vec!["c0", "c1", "", "", "", "", "", "", "EDIT", "status"],
        "the transcript continues at the top and the editor sits at the bottom"
    );
}

/// A live area of `fill` rows of box.
fn draw_box(screen: &mut Screen, style: u32, fill: u16) {
    screen.clear();
    for y in 0..fill {
        screen.put_str(0, y, &format!("BOX{y}"), style);
    }
}

#[tokio::test]
async fn close_erases_the_live_area_and_parks_the_cursor_under_the_transcript() {
    let columns = 10;
    let rows = 10;
    let mut term = Vt100Terminal::new(rows, columns, 16);
    let mut screen = Screen::inline(columns, rows, 2);
    let mut out = Vec::new();
    let style = screen.default_style();

    draw_live_area(&mut screen, style);
    render(&mut screen, &mut term, &mut out).await;

    screen.close(&mut out);
    term.write_frame(&out).await.expect("write frame");

    assert_eq!(
        term.screen().cursor_position(),
        (rows - 2, 0),
        "the cursor sits at the first live row, column 0"
    );
    let screen_rows = all_rows(&term, columns);
    assert!(
        screen_rows[usize::from(rows) - 2..]
            .iter()
            .all(String::is_empty),
        "the live area is erased: {screen_rows:?}"
    );
}

/// A live area of two plain rows.
fn draw_live_area(screen: &mut Screen, style: u32) {
    screen.clear();
    screen.put_str(0, 0, "input", style);
    screen.put_str(0, 1, "status", style);
}

/// Commit plain lines in one batch, the way the app hands over a finished
/// block of a reply.
fn commit_plain<'a>(screen: &mut Screen, texts: &'a [&'a str]) {
    let (runs, spans) = lines(texts);
    screen.commit(&runs, &spans);
}

/// Commit one block of plain lines that the ring can draw again: the fold
/// state's number for it and the source its re-render starts from.
fn commit_block(screen: &mut Screen, fold: u64, texts: &[&str]) {
    let (runs, spans) = lines(texts);
    let source: Source = Arc::new(());
    screen.commit_blocks(
        &runs,
        &spans,
        &[CommitBlock {
            fold,
            lines: 0..texts.len(),
            open: false,
            source,
        }],
    );
}

/// Draw the block `fold` again with the plain lines `texts`, as the fold key
/// that opened it does.
fn reopen_block(screen: &mut Screen, fold: u64, open: bool, texts: &[&str]) {
    let (runs, spans) = lines(texts);
    assert!(
        screen.replace_block(fold, open, &runs, &spans),
        "the ring still holds block {fold}"
    );
}

/// Commit `count` lines named `line 0` .. `line {count-1}` in one batch.
fn fill_transcript(screen: &mut Screen, count: usize) {
    let texts: Vec<String> = (0..count).map(|index| format!("line {index}")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    commit_plain(screen, &refs);
}

/// One frame of full screen: the app draws the transcript from the ring into
/// the back grid, and the diff writes the frame in one write.
async fn draw_transcript_frame(screen: &mut Screen, term: &mut Vt100Terminal, out: &mut Vec<u8>) {
    screen.sync();
    screen.draw_transcript();
    render(screen, term, out).await;
}

#[tokio::test]
async fn enter_and_leave_restore_the_normal_screen() {
    let columns = 60;
    let rows = 6;
    let mut term = Vt100Terminal::new(rows, columns, 16);
    let mut screen = Screen::full(columns, rows);
    let mut out = Vec::new();
    screen.set_session("abc12345");

    term.write_frame(b"shell prompt\r\nsecond\r\n")
        .await
        .expect("the normal screen holds the shell's lines");
    let normal = all_rows(&term, columns);

    screen.enter(&mut out);
    term.write_frame(&out).await.expect("enter");
    assert!(
        term.screen().alternate_screen(),
        "full screen runs in the alternate screen"
    );

    commit_plain(&mut screen, &["transcript line"]);
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    assert_eq!(
        all_rows(&term, columns)[0],
        "transcript line",
        "the full screen draws the whole screen from the grid"
    );

    screen.close(&mut out);
    term.write_frame(&out).await.expect("leave");
    assert!(
        !term.screen().alternate_screen(),
        "leaving full screen gives the normal screen back"
    );
    let after = all_rows(&term, columns);
    assert_eq!(
        &after[..2],
        &normal[..2],
        "the lines the shell wrote come back unchanged: {after:?}"
    );
    assert_eq!(
        after[2], "reagent: session ABC12345 · reagent -c to continue",
        "the exit line lands in the scrollback right under them: {after:?}"
    );
}

#[tokio::test]
async fn a_narrow_transcript_area_leaves_the_sidebar_alone() {
    let columns = 20;
    let rows = 4;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::full(columns, rows);
    let mut out = Vec::new();
    let style = screen.default_style();

    screen.set_transcript_area(0, 0, 12, rows);
    commit_plain(&mut screen, &["a line that", "fits here"]);
    screen.sync();
    // The sidebar keeps its columns while the transcript draws over its own.
    for y in 0..rows {
        screen.put_str(12, y, "SIDEBAR", style);
    }
    screen.draw_transcript();
    render(&mut screen, &mut term, &mut out).await;

    let screen_rows = all_rows(&term, columns);
    assert_eq!(
        screen_rows[0], "a line that SIDEBAR",
        "the transcript draws only up to the area's right edge: {screen_rows:?}"
    );
    assert_eq!(
        &screen_rows[1][..12],
        "fits here   ",
        "the transcript blanks only its own columns: {screen_rows:?}"
    );
    assert_eq!(
        &screen_rows[3][..12],
        "            ",
        "a row past the transcript's last line stays blank: {screen_rows:?}"
    );
    assert_eq!(
        &screen_rows[1][12..],
        "SIDEBAR",
        "the sidebar cells beside it survive: {screen_rows:?}"
    );
}

/// A stored line wider than the area draws on the rows after it, in order, so
/// nothing of it is lost when the area narrows.
#[tokio::test]
async fn a_line_wider_than_the_area_takes_the_rows_after_it() {
    let columns = 120;
    let rows = 6;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::full(columns, rows);
    let mut out = Vec::new();
    let line = "x".repeat(150);
    commit_plain(&mut screen, &[line.as_str()]);
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;

    let drawn = all_rows(&term, columns);
    assert_eq!(
        drawn[0],
        "x".repeat(120),
        "the first row holds the first 120 cells: {drawn:?}"
    );
    assert_eq!(
        drawn[1],
        "x".repeat(30),
        "the other 30 cells follow on the next row: {drawn:?}"
    );
    assert!(
        drawn[2..].iter().all(|row| row.is_empty()),
        "the rows after it stay blank: {drawn:?}"
    );
}

/// A narrower area wraps the lines the ring already holds, and a viewport the
/// user scrolled up keeps the stored line it started on at its top.
#[tokio::test]
async fn a_narrower_area_keeps_the_top_line_of_a_scrolled_viewport() {
    let columns = 120;
    let rows = 6;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::full(columns, rows);
    let mut out = Vec::new();
    let texts: Vec<String> = (0..30).map(|index| format!("line {index:05}")).collect();
    let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
    commit_plain(&mut screen, &texts);
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    screen.transcript_mut().scroll_up(1);
    assert_eq!(
        screen.transcript().top(),
        23,
        "one line up from the bottom page"
    );

    // Six columns hold half a line, so every stored line takes two rows now.
    screen.set_transcript_area(0, 0, 6, rows);
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    assert_eq!(
        screen.transcript().top(),
        23,
        "the line the viewport started on is still its top"
    );
    assert_eq!(
        all_rows(&term, columns)
            .iter()
            .map(|row| row[..6].trim_end().to_string())
            .collect::<Vec<String>>(),
        ["line 0", "0023", "line 0", "0024", "line 0", "0025"],
        "the wrapped rows of the lines below it"
    );
}

#[tokio::test]
async fn a_page_up_shows_the_page_above_a_long_transcript() {
    let columns = 12;
    let rows = 6;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::full(columns, rows);
    let mut out = Vec::new();
    fill_transcript(&mut screen, 5_000);

    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    assert_eq!(
        all_rows(&term, columns),
        [
            "line 4994",
            "line 4995",
            "line 4996",
            "line 4997",
            "line 4998",
            "line 4999"
        ],
        "the viewport follows the lines that arrive"
    );

    screen.transcript_mut().page_up();
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    assert_eq!(
        all_rows(&term, columns),
        [
            "line 4988",
            "line 4989",
            "line 4990",
            "line 4991",
            "line 4992",
            "line 4993"
        ],
        "one page up shows the page above"
    );
    assert!(
        screen.diff_rows_visited() <= rows,
        "a scroll frame draws only the rows that show"
    );
}

#[test]
fn a_scroll_frame_visits_the_same_lines_for_500_and_5_000_stored() {
    let mut visited = Vec::new();
    for count in [500usize, 5_000] {
        let mut screen = Screen::full(12, 6);
        let mut out = Vec::new();
        fill_transcript(&mut screen, count);
        screen.sync();
        screen.draw_transcript();
        screen.diff(&mut out);

        screen.transcript_mut().page_up();
        screen.sync();
        screen.draw_transcript();
        screen.diff(&mut out);
        visited.push((
            screen.transcript().lines_visited(),
            screen.diff_rows_visited(),
        ));

        // A width change wraps the whole ring once; the frame after it still
        // draws only the rows that show.
        screen.set_transcript_area(0, 0, 5, 6);
        screen.sync();
        screen.draw_transcript();
        screen.diff(&mut out);
        screen.transcript_mut().scroll_up(1);
        screen.sync();
        screen.draw_transcript();
        screen.diff(&mut out);
        visited.push((
            screen.transcript().lines_visited(),
            screen.diff_rows_visited(),
        ));
    }
    assert_eq!(
        visited[0], visited[2],
        "a scroll frame costs the same for 500 and 5,000 stored lines: {visited:?}"
    );
    assert_eq!(
        visited[0], visited[1],
        "a scroll frame draws the same rows for 500 and 5,000 stored: {visited:?}"
    );
    assert!(
        visited[0].0 <= 6,
        "a scroll frame visits only the visible lines: {visited:?}"
    );
}

#[test]
fn a_scroll_frame_reuses_every_buffer() {
    let mut screen = Screen::full(12, 6);
    fill_transcript(&mut screen, 100);
    let mut out = Vec::new();
    screen.sync();
    screen.draw_transcript();
    screen.diff(&mut out);

    let before = (
        out.capacity(),
        screen.back().cells_capacity(),
        screen.back().dirty_capacity(),
        screen.transcript().capacity(),
    );
    for _ in 0..100 {
        screen.transcript_mut().scroll_up(1);
        screen.sync();
        screen.draw_transcript();
        screen.diff(&mut out);
    }
    let after = (
        out.capacity(),
        screen.back().cells_capacity(),
        screen.back().dirty_capacity(),
        screen.transcript().capacity(),
    );
    assert_eq!(
        before, after,
        "a scroll frame changed a buffer capacity, so it allocated"
    );
}

#[test]
fn follow_mode_sticks_to_the_bottom_until_a_scroll_up() {
    let mut screen = Screen::full(12, 4);
    fill_transcript(&mut screen, 10);
    assert!(screen.transcript().follow(), "a new transcript follows");
    assert_eq!(
        screen.transcript().top(),
        6,
        "the viewport sits at the bottom"
    );

    commit_plain(&mut screen, &["line 10"]);
    assert_eq!(
        screen.transcript().top(),
        7,
        "a line that arrives moves the viewport while it follows"
    );

    screen.transcript_mut().scroll_up(2);
    assert!(!screen.transcript().follow(), "a scroll up ends following");
    assert_eq!(screen.transcript().top(), 5);
    commit_plain(&mut screen, &["line 11"]);
    assert_eq!(
        screen.transcript().top(),
        5,
        "the view holds still while lines arrive"
    );

    screen.transcript_mut().scroll_down(1);
    assert!(
        !screen.transcript().follow(),
        "a view above the bottom does not follow"
    );
    screen.transcript_mut().scroll_down(10);
    assert!(
        screen.transcript().follow(),
        "the viewport at the bottom follows again"
    );
    commit_plain(&mut screen, &["line 12"]);
    assert_eq!(
        screen.transcript().top(),
        9,
        "the viewport is back at the bottom"
    );
}

/// The bytes of an exit write that follow the end of the alternate screen.
fn after_leaving(out: &[u8]) -> &[u8] {
    let at = out
        .windows(ALT_SCREEN_OFF.len())
        .position(|window| window == ALT_SCREEN_OFF)
        .expect("the exit write leaves the alternate screen");
    &out[at + ALT_SCREEN_OFF.len()..]
}

/// Leaving full screen prints one line that names the session and the command
/// that continues it, and none of the transcript.
#[tokio::test]
async fn exit_line_names_the_session_and_prints_no_transcript() {
    let columns = 60;
    let rows = 6;
    let mut term = Vt100Terminal::new(rows, columns, 16);
    let mut screen = Screen::full(columns, rows);
    let mut out = Vec::new();
    screen.set_session("20260101-abcd-4f2e-9c5d");

    term.write_frame(b"shell prompt\r\n")
        .await
        .expect("the normal screen holds the shell's line");
    screen.enter(&mut out);
    term.write_frame(&out).await.expect("enter");
    commit_plain(&mut screen, &["line one", "line two"]);
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;

    let writes = term.writes();
    screen.close(&mut out);
    term.write_frame(&out).await.expect("leave");
    assert_eq!(
        term.writes(),
        writes + 1,
        "the exit line leaves in one write"
    );

    let line = String::from_utf8(after_leaving(&out).to_vec()).expect("the exit line is text");
    assert_eq!(
        line, "\r\x1b[2mreagent:\x1b[0m session 20260101 \u{b7} reagent -c to continue\r\n",
        "the exit line starts at the column of a clean line, holds the dim name, the session and the flag, and ends with a newline"
    );

    let after = all_rows(&term, columns);
    assert_eq!(after[0], "shell prompt");
    assert_eq!(
        after[1], "reagent: session 20260101 \u{b7} reagent -c to continue",
        "the line lands under the shell's own: {after:?}"
    );
    assert!(
        after[2..].iter().all(String::is_empty),
        "no transcript line reaches the scrollback: {after:?}"
    );
    assert!(
        term.screen().cell(1, 0).is_some_and(|cell| cell.dim()),
        "the name that starts the line is dim"
    );
    assert_eq!(
        term.screen().cursor_position(),
        (2, 0),
        "the cursor starts a clean line under the exit line"
    );
    assert!(
        screen.transcript().is_empty(),
        "the lines of one full-screen run outlive it nowhere"
    );
}

/// Inline mode keeps its lines in the scrollback and prints no exit line.
#[tokio::test]
async fn exit_line_inline_mode_is_unchanged() {
    let columns = 40;
    let rows = 8;
    let mut term = Vt100Terminal::new(rows, columns, 16);
    let mut screen = Screen::inline(columns, rows, 2);
    let mut out = Vec::new();
    screen.set_session("20260101-abcd-4f2e-9c5d");

    commit_plain(&mut screen, &["line one", "line two"]);
    render(&mut screen, &mut term, &mut out).await;
    screen.close(&mut out);
    term.write_frame(&out).await.expect("leave");

    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(
        !text.contains("reagent:"),
        "inline mode prints no exit line: {text:?}"
    );
    assert!(
        !text.contains("session"),
        "inline mode names no session: {text:?}"
    );
    let after = all_rows(&term, columns);
    let first = after
        .iter()
        .position(|row| row == "line one")
        .unwrap_or_else(|| panic!("the committed lines stay: {after:?}"));
    assert_eq!(
        after[first + 1],
        "line two",
        "the lines keep their order in the scrollback: {after:?}"
    );
    assert_eq!(
        term.screen().cursor_position(),
        (rows - 2, 0),
        "the close parks the cursor under the transcript"
    );
}

#[tokio::test]
async fn the_ring_caps_at_twenty_thousand_lines_and_names_the_dropped_ones() {
    let columns = 20;
    let rows = 4;
    let mut term = Vt100Terminal::new(rows, columns, 0);
    let mut screen = Screen::full(columns, rows);
    let mut out = Vec::new();
    fill_transcript(&mut screen, TRANSCRIPT_RING_LINES + 10);

    assert_eq!(
        screen.transcript().ring_len(),
        TRANSCRIPT_RING_LINES,
        "the ring keeps at most {TRANSCRIPT_RING_LINES} lines"
    );
    assert_eq!(screen.transcript().dropped(), 10, "ten lines left the ring");

    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    assert_eq!(
        all_rows(&term, columns),
        ["line 20006", "line 20007", "line 20008", "line 20009"],
        "the bottom of the transcript shows no line that names the dropped ones"
    );

    screen.transcript_mut().scroll_up(u64::MAX);
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    assert_eq!(
        all_rows(&term, columns),
        ["... 10 earlier lines", "line 10", "line 11", "line 12"],
        "the top of the ring names the lines that left it"
    );
    assert!(
        term.screen().cell(0, 0).is_some_and(|cell| cell.dim()),
        "the line that names the lines left out is dim"
    );
}

/// The text and the style of every run of one stored line, read from the ring.
fn stored_line(screen: &Screen, index: u64) -> (String, Vec<u32>) {
    let mut text = String::new();
    let mut styles = Vec::new();
    for (part, style) in screen.transcript().runs_of(index) {
        text.push_str(part);
        styles.push(style);
    }
    (text, styles)
}

/// One line of a transcript whose runs differ in size and style: a run named
/// after the line, then a bold run on every third line and a dim one on every
/// fifth.
fn mixed_segments(index: usize, bold: u32, dim: u32) -> Vec<(String, u32)> {
    let mut segments = vec![(format!("L{index}"), 0)];
    if index.is_multiple_of(3) {
        segments.push((" b".to_string(), bold));
    }
    if index.is_multiple_of(5) {
        segments.push((" d".to_string(), dim));
    }
    segments
}

/// The segments of every line of `mixed_segments`.
fn mixed_lines(count: usize, bold: u32, dim: u32) -> Vec<Vec<(String, u32)>> {
    (0..count)
        .map(|index| mixed_segments(index, bold, dim))
        .collect()
}

/// The runs and the line ranges of `segments`, borrowing their texts.
fn mixed_runs<'a>(
    segments: &'a [Vec<(String, u32)>],
) -> (Vec<Run<'a>>, Vec<std::ops::Range<usize>>) {
    let mut runs = Vec::new();
    let mut spans = Vec::new();
    for line in segments {
        let start = runs.len();
        for (text, style) in line {
            runs.push(Run {
                text: text.as_str(),
                style: *style,
            });
        }
        spans.push(start..runs.len());
    }
    (runs, spans)
}

#[tokio::test]
async fn a_full_ring_keeps_the_text_and_styles_of_its_first_and_last_lines() {
    let columns = 24;
    let mut screen = Screen::full(columns, 6);
    let bold = screen.intern(Style {
        bold: true,
        ..Style::default()
    });
    let dim = screen.transcript().dim_style();
    assert_ne!(bold, dim, "the bold and dim styles are different ids");

    let count = TRANSCRIPT_RING_LINES + 10;
    let segments = mixed_lines(count, bold, dim);
    let (runs, spans) = mixed_runs(&segments);
    screen.commit(&runs, &spans);
    assert_eq!(
        screen.transcript().ring_len(),
        TRANSCRIPT_RING_LINES,
        "the ring keeps at most {TRANSCRIPT_RING_LINES} lines"
    );
    assert_eq!(screen.transcript().dropped(), 10, "ten lines left the ring");

    // The first and the last stored line still read back with the text and the
    // styles they were committed with, though the byte buffer of the ring was
    // compacted many times.
    for index in [10usize, count - 1] {
        let (text, styles) = stored_line(&screen, index as u64);
        let expected: String = segments[index]
            .iter()
            .map(|(part, _)| part.as_str())
            .collect();
        assert_eq!(text, expected, "line {index} reads back from the ring");
        let expected_styles: Vec<u32> = segments[index].iter().map(|(_, style)| *style).collect();
        assert_eq!(styles, expected_styles, "line {index} keeps its run styles");
    }

    // The top of the ring draws them, and the line that names the dropped ones
    // is dim.
    let mut term = Vt100Terminal::new(6, columns, 0);
    let mut out = Vec::new();
    screen.transcript_mut().scroll_up(u64::MAX);
    draw_transcript_frame(&mut screen, &mut term, &mut out).await;
    let rows = all_rows(&term, columns);
    let line_text = |index: usize| -> String {
        segments[index]
            .iter()
            .map(|(part, _)| part.as_str())
            .collect()
    };
    assert_eq!(
        rows[0], "... 10 earlier lines",
        "the top names the dropped ones"
    );
    assert_eq!(rows[1], line_text(10), "the first stored line draws");
    assert_eq!(rows[3], line_text(12), "the bold line under it draws");
    assert!(
        term.screen().cell(0, 0).is_some_and(|cell| cell.dim()),
        "the line that names the lines left out is dim"
    );
    assert!(
        term.screen().cell(3, 4).is_some_and(|cell| cell.bold()),
        "a stored run keeps its bold style after the ring was compacted"
    );

    // Leaving the run drops the ring: the transcript of a full screen outlives
    // it nowhere.
    screen.close(&mut out);
    assert!(
        screen.transcript().is_empty(),
        "the close drops the lines of the run"
    );
}

#[test]
fn a_full_ring_commits_without_allocating() {
    let mut screen = Screen::full(24, 6);
    let warm: Vec<String> = (0..2 * TRANSCRIPT_RING_LINES + 10_000)
        .map(|index| format!("line {index}"))
        .collect();
    let refs: Vec<&str> = warm.iter().map(String::as_str).collect();
    commit_plain(&mut screen, &refs);
    let before = screen.transcript().capacity();
    let mut committed = refs.len();

    // Ten batches of 1,000 lines: the ring is full and warm, so the oldest
    // bytes leave with the oldest line and every buffer keeps its room.
    for batch in 0..10 {
        let more: Vec<String> = (0..1_000)
            .map(|index| format!("line {}", committed + index))
            .collect();
        let more_refs: Vec<&str> = more.iter().map(String::as_str).collect();
        commit_plain(&mut screen, &more_refs);
        committed += more_refs.len();
        assert_eq!(
            screen.transcript().capacity(),
            before,
            "batch {batch} past the ring cap changed a capacity, so it allocated"
        );
    }
    assert_eq!(
        screen.transcript().dropped(),
        (committed - TRANSCRIPT_RING_LINES) as u64,
        "every line past the cap left the ring"
    );
}

/// A block the fold key opens takes the room of its new lines: the lines after
/// it move once, on the key, and the view keeps the line it started on.
#[test]
fn a_reopened_block_takes_the_room_of_its_new_lines_and_keeps_the_top() {
    let mut screen = Screen::full(20, 3);
    commit_block(&mut screen, 1, &["a0", "a1"]);
    commit_block(&mut screen, 2, &["b0", "b1"]);
    commit_block(&mut screen, 3, &["c0", "c1"]);
    assert_eq!(
        screen.transcript().top(),
        3,
        "the viewport follows the bottom"
    );
    screen.transcript_mut().scroll_up(1);
    assert_eq!(
        screen.transcript().top(),
        2,
        "the viewport sits on the second block"
    );
    assert_eq!(
        screen.transcript().block_at(2),
        Some(2),
        "Tab opens the block at the top of the view"
    );
    let before = screen.transcript().lengths();

    reopen_block(&mut screen, 2, true, &["b0", "b1", "b2", "b3", "b4"]);
    assert_eq!(
        screen.transcript().ring_len(),
        9,
        "the block took the room of its five lines"
    );
    assert_eq!(screen.transcript().top(), 2, "the view keeps its top line");
    assert_eq!(
        stored_line(&screen, 2).0,
        "b0",
        "the block's first line stays on its line"
    );
    assert_eq!(
        stored_line(&screen, 4).0,
        "b2",
        "a line the block gained sits in place"
    );
    assert_eq!(
        stored_line(&screen, 7).0,
        "c0",
        "the lines below the block moved down"
    );
    assert_eq!(screen.transcript().block_at(7), Some(3));
    assert_eq!(
        screen.transcript().rendered_open(2),
        Some(true),
        "the ring knows the block is open now"
    );
    // The ring stored the three new lines and no copy of itself: three runs and
    // their bytes, and three more lines.
    let after = screen.transcript().lengths();
    assert_eq!(after.0 - before.0, 6, "three runs of two bytes");
    assert_eq!(after.1 - before.1, 3, "three runs");
    assert_eq!(after.2 - before.2, 3, "three lines");

    // Folding it back takes the room away again.
    reopen_block(&mut screen, 2, false, &["b0", "b1"]);
    assert_eq!(screen.transcript().ring_len(), 6);
    assert_eq!(
        stored_line(&screen, 4).0,
        "c0",
        "the lines below moved back up"
    );
    assert_eq!(
        screen.transcript().top(),
        2,
        "the top line is the same line number, the block is shorter"
    );
}

/// A viewport that follows the bottom stays at the bottom when a block above it
/// opens.
#[test]
fn an_opened_block_while_the_view_follows_stays_at_the_bottom() {
    let mut screen = Screen::full(20, 4);
    commit_block(&mut screen, 1, &["a0", "a1"]);
    commit_block(&mut screen, 2, &["b0", "b1"]);
    commit_block(&mut screen, 3, &["c0", "c1"]);
    assert_eq!(
        screen.transcript().top(),
        2,
        "the viewport follows the bottom"
    );

    reopen_block(&mut screen, 1, true, &["a0", "a1", "a2", "a3", "a4"]);
    assert!(screen.transcript().follow(), "the view still follows");
    assert_eq!(
        screen.transcript().top(),
        5,
        "the viewport moved to the new bottom"
    );
}

/// The lines a block lost to the ring leave the block behind: its key does
/// nothing, while a block the ring still holds opens.
#[test]
fn a_block_that_left_the_ring_cannot_be_drawn_again() {
    let mut screen = Screen::full(20, 4);
    commit_block(&mut screen, 1, &["a0", "a1"]);
    commit_block(&mut screen, 2, &["b0", "b1", "b2", "b3"]);
    let filled = TRANSCRIPT_RING_LINES - 4;
    fill_transcript(&mut screen, filled);
    assert_eq!(
        screen.transcript().dropped(),
        2,
        "the first block's lines left the ring"
    );
    assert!(screen.transcript().source(1).is_none(), "the block is gone");
    assert_eq!(screen.transcript().block_at(0), None);
    let (runs, spans) = lines(&["a0", "a1", "a2"]);
    assert!(
        !screen.replace_block(1, true, &runs, &spans),
        "the key of a block that left the ring does nothing"
    );
    assert_eq!(
        screen.transcript().newest_block(),
        Some(2),
        "the block the ring still holds stays"
    );
    assert!(
        screen.replace_block(2, true, &runs, &spans),
        "the block the ring still holds opens"
    );
}

/// The frames after a reopen draw the ring as they found it: no buffer of the
/// screen grows, so a reopen costs nothing per later frame.
#[test]
fn a_frame_after_a_reopen_reuses_every_buffer() {
    let mut screen = Screen::full(20, 6);
    commit_block(&mut screen, 1, &["a0", "a1"]);
    fill_transcript(&mut screen, 100);
    reopen_block(&mut screen, 1, true, &["a0", "a1", "a2", "a3"]);
    let mut out = Vec::new();
    screen.sync();
    screen.draw_transcript();
    screen.diff(&mut out);

    let before = (
        out.capacity(),
        screen.back().cells_capacity(),
        screen.back().dirty_capacity(),
        screen.transcript().capacity(),
    );
    for _ in 0..20 {
        screen.sync();
        screen.draw_transcript();
        screen.diff(&mut out);
    }
    let after = (
        out.capacity(),
        screen.back().cells_capacity(),
        screen.back().dirty_capacity(),
        screen.transcript().capacity(),
    );
    assert_eq!(
        before, after,
        "a frame after a reopen changed a buffer capacity, so it allocated"
    );
}

/// A block that opens in a full ring keeps the ring at its cap: the oldest
/// lines leave it, and the block itself stays.
#[test]
fn a_block_that_opens_in_a_full_ring_drops_the_oldest_lines() {
    let mut screen = Screen::full(20, 6);
    fill_transcript(&mut screen, TRANSCRIPT_RING_LINES);
    commit_block(&mut screen, 1, &["a0", "a1"]);
    assert_eq!(screen.transcript().ring_len(), TRANSCRIPT_RING_LINES);
    assert_eq!(screen.transcript().dropped(), 2, "two lines left the ring");

    reopen_block(
        &mut screen,
        1,
        true,
        &["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9"],
    );
    assert_eq!(
        screen.transcript().ring_len(),
        TRANSCRIPT_RING_LINES,
        "the ring stayed at its cap"
    );
    assert_eq!(
        screen.transcript().dropped(),
        10,
        "the eight lines the block gained pushed eight more out"
    );
    assert_eq!(
        screen.transcript().newest_block(),
        Some(1),
        "the block that opened is still in the ring"
    );
    assert_eq!(
        stored_line(&screen, 20_009).0,
        "a9",
        "the block's last line is the ring's last line"
    );
    assert_eq!(
        stored_line(&screen, 20_000).0,
        "a0",
        "the block's first line keeps its text"
    );
    assert_eq!(
        stored_line(&screen, 19_999).0,
        "line 19999",
        "the line before the block keeps its text"
    );
    assert_eq!(
        stored_line(&screen, 10).0,
        "line 10",
        "the oldest line the ring holds keeps its text"
    );
}

/// The source of a block reads back by the line it drew, and the ring keeps it
/// for as long as it keeps that line.
#[test]
fn block_source_reads_back_by_its_line() {
    let mut screen = Screen::full(20, 4);
    commit_plain(&mut screen, &["one", "two"]);
    screen.set_origin(0, 2, "one\ntwo\n", false, false);
    for line in 0..2 {
        let origin = screen
            .origin(line)
            .unwrap_or_else(|| panic!("a source for line {line}"));
        assert_eq!(screen.origin_text(&origin), "one\ntwo\n");
        assert_eq!((origin.line, origin.lines), (0, 2));
    }
    assert!(screen.origin(2).is_none(), "a line past the block has none");
    assert!(
        screen.origin(0).is_some_and(|origin| !origin.code),
        "a paragraph is no code block"
    );
}

/// A ring that drops every line of a block drops its source too, and gives its
/// bytes back.
#[test]
fn a_full_ring_frees_block_source() {
    let mut screen = Screen::full(20, 4);
    commit_plain(&mut screen, &["one"]);
    screen.set_origin(0, 1, "one\n", false, false);
    assert_eq!(screen.origin_text(&screen.origin(0).unwrap()), "one\n");
    let filled = TRANSCRIPT_RING_LINES;
    fill_transcript(&mut screen, filled);
    assert!(screen.transcript().dropped() > 0, "the oldest line left");
    assert!(
        screen.origin(0).is_none(),
        "the source of the line that left the ring is gone"
    );
    assert_eq!(
        screen.transcript().source_len(),
        0,
        "the arena gives its bytes back"
    );
}

/// A block that opens above the sources moves their lines with it, so a copy of
/// a row still reads the text the reply wrote.
#[test]
fn a_fold_above_keeps_block_source_in_place() {
    let mut screen = Screen::full(20, 8);
    commit_block(&mut screen, 1, &["a0", "a1"]);
    commit_plain(&mut screen, &["b0", "b1"]);
    screen.set_origin(2, 2, "b0\nb1\n", false, false);
    reopen_block(&mut screen, 1, true, &["a0", "a1", "a2", "a3", "a4"]);
    let origin = screen
        .origin(5)
        .expect("the source moved down with its lines");
    assert_eq!(screen.origin_text(&origin), "b0\nb1\n");
    assert_eq!(origin.line, 5);
    assert!(
        screen.origin(4).is_none(),
        "the lines the block grew take none"
    );
    reopen_block(&mut screen, 1, false, &["a0"]);
    let origin = screen
        .origin(2)
        .expect("the source moved up with its lines");
    assert_eq!(screen.origin_text(&origin), "b0\nb1\n");
}

/// The 64 by 48 PNG the picture tests draw, base64 as the journal holds it.
const GRADIENT: &[u8] = include_bytes!("../../../image/tests/fixtures/gradient-64x48.png");

/// The image of the fixture PNG, as a registered picture holds it.
fn picture_source() -> crate::picture::Source {
    let data = base64::engine::general_purpose::STANDARD.encode(GRADIENT);
    crate::picture::Source {
        bytes: GRADIENT.len(),
        png: true,
        data: data.into(),
    }
}

/// How many times `needle` sits in `bytes`.
fn count(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// Whether `needle` sits in `bytes`.
fn holds(bytes: &[u8], needle: &[u8]) -> bool {
    count(bytes, needle) > 0
}

/// A full screen of `rows` rows with the protocol `native` on a ring of `texts`,
/// where `first` names the line the picture of `height` rows is registered on.
fn pictured(rows: u16, native: Option<Native>, texts: &[&str], first: u64, height: u16) -> Screen {
    let mut screen = Screen::full(40, rows);
    screen.set_native(native);
    commit_plain(&mut screen, texts);
    screen.draw_transcript();
    screen.add_picture(first, height, 2, 0, picture_source());
    screen
}

#[test]
fn native_picture_iterm2_draws_once_whole() {
    let mut screen = pictured(
        24,
        Some(Native {
            protocol: Protocol::Iterm2,
            tmux: false,
        }),
        &["a", "b", "c", "▀▄", "▄▀"],
        3,
        2,
    );
    let mut out = Vec::new();
    screen.diff(&mut out);
    assert!(
        !holds(&out, b"]1337;"),
        "the frame draws the cells only: {out:?}"
    );
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert_eq!(count(&images, b"]1337;File=inline=1;"), 1, "{images:?}");
    let cup = images
        .windows(6)
        .position(|window| window == b"\x1b[4;1H")
        .expect("the cursor moves to the picture's first cell");
    let inline: &[u8] = b"]1337;File=inline=";
    let image = images
        .windows(inline.len())
        .position(|window| window == inline)
        .expect("the image follows the cursor");
    assert!(cup < image, "the CUP comes first: {images:?}");
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert!(images.is_empty(), "nothing changed");
}

#[test]
fn native_picture_unknown_terminal_sends_nothing() {
    let mut screen = pictured(24, None, &["▀▄", "▄▀"], 0, 2);
    let mut out = Vec::new();
    screen.diff(&mut out);
    assert!(holds(&out, "▀▄".as_bytes()), "the cells are drawn");
    assert!(!holds(&out, b"1337"), "{out:?}");
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert!(images.is_empty(), "no image ever");
    assert!(!screen.pictures_due());
}

#[test]
fn native_picture_scroll_cut_removes_it() {
    let mut screen = Screen::full(40, 4);
    screen.set_native(Some(Native {
        protocol: Protocol::Iterm2,
        tmux: false,
    }));
    commit_plain(&mut screen, &["0", "1", "2", "3", "▀▄", "5", "6", "7", "8"]);
    screen.transcript_mut().scroll_up(1);
    screen.draw_transcript();
    screen.add_picture(4, 2, 2, 0, picture_source());
    let mut out = Vec::new();
    screen.diff(&mut out);
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert_eq!(count(&images, b"]1337;File=inline=1;"), 1, "{images:?}");
    // One line down and the picture's first row leaves the viewport.
    screen.transcript_mut().scroll_down(1);
    screen.draw_transcript();
    let mut out = Vec::new();
    screen.diff(&mut out);
    assert!(holds(&out, "5".as_bytes()), "the row under it: {out:?}");
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert!(images.is_empty(), "no image: {images:?}");
}

#[test]
fn native_picture_sheet_blocks_it() {
    let mut screen = pictured(
        24,
        Some(Native {
            protocol: Protocol::Iterm2,
            tmux: false,
        }),
        &["a", "▀▄", "▄▀"],
        1,
        2,
    );
    let mut out = Vec::new();
    screen.diff(&mut out);
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert_eq!(count(&images, b"]1337;File=inline=1;"), 1);
    screen.block_pictures(true);
    let mut out = Vec::new();
    screen.diff(&mut out);
    assert!(holds(&out, "▀▄".as_bytes()), "the cells again: {out:?}");
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert!(images.is_empty(), "a blocked picture waits: {images:?}");
    screen.block_pictures(false);
    let mut out = Vec::new();
    screen.diff(&mut out);
    assert!(screen.pictures_due(), "the picture wants its image back");
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert_eq!(count(&images, b"]1337;File=inline=1;"), 1, "{images:?}");
}

#[test]
fn native_picture_ninth_drops_the_oldest() {
    let mut screen = Screen::full(40, 20);
    screen.set_native(Some(Native {
        protocol: Protocol::Iterm2,
        tmux: false,
    }));
    let texts: Vec<&str> = vec!["a"; 12];
    commit_plain(&mut screen, &texts);
    screen.draw_transcript();
    for line in 0..9 {
        screen.add_picture(line, 1, 2, 0, picture_source());
    }
    let mut out = Vec::new();
    screen.diff(&mut out);
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert_eq!(count(&images, b"]1337;File=inline=1;"), 8, "{images:?}");
    assert!(!holds(&images, b"\x1b[1;1H"), "the first one dropped");
    assert!(holds(&images, b"\x1b[2;1H"), "the second one kept");
}

#[test]
fn native_picture_write_beside_it_keeps_it() {
    let mut screen = pictured(
        24,
        Some(Native {
            protocol: Protocol::Iterm2,
            tmux: false,
        }),
        &["a", "b", "c", "▀▄", "▄▀"],
        3,
        2,
    );
    let mut out = Vec::new();
    screen.diff(&mut out);
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert!(holds(&images, b"]1337;"), "the image is on screen");
    let style = screen.default_style();
    screen.put_str(20, 3, "tick", style);
    screen.diff(&mut out);
    assert!(holds(&out, b"tick"), "the write went out: {out:?}");
    assert!(
        !screen.pictures_due(),
        "a write beside the image leaves it on screen"
    );
    screen.put_str(0, 3, "xy", style);
    screen.diff(&mut out);
    assert!(
        screen.pictures_due(),
        "a write over the image takes it off, so it goes out again"
    );
}

#[test]
fn native_picture_blanks_its_cells_before_the_image() {
    let mut screen = pictured(
        24,
        Some(Native {
            protocol: Protocol::Iterm2,
            tmux: false,
        }),
        &["a", "b", "c", "▀▄", "▄▀"],
        3,
        2,
    );
    let mut out = Vec::new();
    screen.diff(&mut out);
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert!(
        holds(&images, b"\x1b[4;1H  ") && holds(&images, b"\x1b[5;1H  "),
        "both rows go blank: {images:?}"
    );
    let spaces = images
        .windows(8)
        .position(|window| window == b"\x1b[5;1H  ")
        .expect("the blank row");
    let image = images
        .windows(6)
        .position(|window| window == b"]1337;")
        .expect("the image");
    assert!(spaces < image, "the cells go blank before the image");
}

#[test]
fn native_picture_moves_when_a_block_above_opens() {
    let mut screen = Screen::full(40, 24);
    screen.set_native(Some(Native {
        protocol: Protocol::Iterm2,
        tmux: false,
    }));
    commit_block(&mut screen, 1, &["thought"]);
    commit_plain(&mut screen, &["▀▄", "▄▀"]);
    screen.add_picture(1, 2, 2, 0, picture_source());
    screen.draw_transcript();
    let mut out = Vec::new();
    screen.diff(&mut out);
    let mut images = Vec::new();
    screen.draw_pictures(&mut images);
    assert!(holds(&images, b"\x1b[2;1H\x1b]1337;"), "{images:?}");
    // The thinking above the picture opens and takes two more lines.
    reopen_block(&mut screen, 1, true, &["thought", "first", "second"]);
    screen.draw_transcript();
    screen.diff(&mut out);
    screen.draw_pictures(&mut images);
    assert!(
        holds(&images, b"\x1b[4;1H\x1b]1337;"),
        "the image follows its cells two rows down: {images:?}"
    );
}
