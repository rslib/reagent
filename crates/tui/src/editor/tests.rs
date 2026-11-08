use std::sync::Arc;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use super::{Editor, MARK, MARK_BYTES, Row};

fn text(editor: &Editor) -> String {
    let (first, second) = editor.chunks();
    let mut out = String::with_capacity(first.len() + second.len());
    out.push_str(first);
    out.push_str(second);
    out
}

fn row_text(editor: &Editor, row: Row) -> String {
    let (first, second) = editor.slice(row.start(), row.end());
    let mut out = String::with_capacity(first.len() + second.len());
    out.push_str(first);
    out.push_str(second);
    out
}

fn visible(editor: &mut Editor, width: u16, height: u16) -> Vec<String> {
    let rows = editor.layout(width, height).rows().to_vec();
    rows.iter().map(|row| row_text(editor, *row)).collect()
}

#[test]
fn inserting_text_leaves_the_cursor_after_it() {
    let mut editor = Editor::new();
    editor.insert("hello");
    assert_eq!(text(&editor), "hello");
    assert_eq!(editor.len(), 5);
    assert_eq!(editor.cursor(), 5);
    assert!(!editor.is_empty());
}

#[test]
fn take_returns_the_text_and_empties_the_editor() {
    let mut editor = Editor::new();
    editor.insert("a\nb");
    assert_eq!(editor.take(), "a\nb");
    assert!(editor.is_empty());
    assert_eq!(editor.cursor(), 0);
    assert_eq!(text(&editor), "");
    assert_eq!(editor.take(), "");
}

#[test]
fn left_and_right_move_one_ascii_char() {
    let mut editor = Editor::new();
    editor.insert("ab");
    editor.left();
    assert_eq!(editor.cursor(), 1);
    editor.left();
    assert_eq!(editor.cursor(), 0);
    editor.left();
    assert_eq!(editor.cursor(), 0, "left at the start stays put");
    editor.right();
    assert_eq!(editor.cursor(), 1);
    editor.right();
    assert_eq!(editor.cursor(), 2);
    editor.right();
    assert_eq!(editor.cursor(), 2, "right at the end stays put");
}

#[test]
fn left_and_right_move_one_wide_char() {
    let mut editor = Editor::new();
    editor.insert("世界");
    assert_eq!(editor.cursor(), 6);
    editor.left();
    assert_eq!(editor.cursor(), 3, "一 wide char is three bytes");
    editor.left();
    assert_eq!(editor.cursor(), 0);
    editor.right();
    assert_eq!(editor.cursor(), 3);
    editor.right();
    assert_eq!(editor.cursor(), 6);
}

#[test]
fn left_and_right_step_over_a_combining_mark() {
    let mut editor = Editor::new();
    editor.insert("e\u{301}x");
    assert_eq!(editor.cursor(), 4);
    editor.left();
    assert_eq!(editor.cursor(), 3);
    editor.left();
    assert_eq!(editor.cursor(), 0, "e + combining accent is one grapheme");
    editor.right();
    assert_eq!(editor.cursor(), 3);
    editor.right();
    assert_eq!(editor.cursor(), 4);
}

#[test]
fn backspace_removes_one_grapheme() {
    let mut editor = Editor::new();
    editor.insert("ab");
    editor.backspace();
    assert_eq!(text(&editor), "a");
    assert_eq!(editor.cursor(), 1);
    editor.backspace();
    editor.backspace();
    assert_eq!(text(&editor), "");
    assert_eq!(editor.cursor(), 0);

    editor.insert("世界");
    editor.backspace();
    assert_eq!(text(&editor), "世");
    assert_eq!(editor.cursor(), 3);

    let mut editor = Editor::new();
    editor.insert("e\u{301}");
    editor.backspace();
    assert_eq!(text(&editor), "");
    assert_eq!(editor.cursor(), 0);
}

#[test]
fn delete_removes_the_grapheme_after_the_cursor() {
    let mut editor = Editor::new();
    editor.insert("世界");
    editor.left();
    editor.left();
    editor.delete();
    assert_eq!(text(&editor), "界");
    assert_eq!(editor.cursor(), 0);
    editor.delete();
    assert_eq!(text(&editor), "");
    editor.delete();
    assert_eq!(text(&editor), "", "delete at the end stays put");

    editor.insert("e\u{301}x");
    let mut editor_at_start = Editor::new();
    editor_at_start.insert("\u{301}");
    editor_at_start.left();
    editor_at_start.delete();
    assert_eq!(text(&editor_at_start), "");
}

#[test]
fn ctrl_a_and_ctrl_e_are_the_ends_of_the_logical_line() {
    let mut editor = Editor::new();
    editor.insert("ab\ncd");
    assert_eq!(editor.cursor(), 5);
    editor.line_start();
    assert_eq!(editor.cursor(), 3);
    editor.line_end();
    assert_eq!(editor.cursor(), 5);
    editor.home(10);
    assert_eq!(editor.cursor(), 3);
    editor.end(10);
    assert_eq!(editor.cursor(), 5, "end of a line is before its newline");
    editor.line_start();
    assert_eq!(editor.cursor(), 3);
    editor.left();
    assert_eq!(editor.cursor(), 2);
    editor.line_start();
    assert_eq!(editor.cursor(), 0);
    editor.line_end();
    assert_eq!(editor.cursor(), 2);
}

#[test]
fn ctrl_k_kills_to_the_end_of_the_line() {
    let mut editor = Editor::new();
    editor.insert("hello world");
    for _ in 0..5 {
        editor.left();
    }
    editor.kill_to_end();
    assert_eq!(text(&editor), "hello ");
    assert_eq!(editor.cursor(), 6);
    editor.kill_to_end();
    assert_eq!(
        text(&editor),
        "hello ",
        "at the end of the line nothing goes"
    );

    let mut editor = Editor::new();
    editor.insert("ab\ncd");
    editor.left();
    editor.kill_to_end();
    assert_eq!(text(&editor), "ab\nc", "a newline ends the line");
    assert_eq!(editor.cursor(), 4);
}

#[test]
fn ctrl_u_kills_to_the_start_of_the_line() {
    let mut editor = Editor::new();
    editor.insert("hello world");
    for _ in 0..5 {
        editor.left();
    }
    editor.kill_to_start();
    assert_eq!(text(&editor), "world");
    assert_eq!(editor.cursor(), 0);
    editor.kill_to_start();
    assert_eq!(
        text(&editor),
        "world",
        "at the start of the line nothing goes"
    );

    let mut editor = Editor::new();
    editor.insert("ab\ncd");
    editor.kill_to_start();
    assert_eq!(text(&editor), "ab\n", "a newline stops the kill");
    assert_eq!(editor.cursor(), 3);
}

#[test]
fn ctrl_w_kills_the_previous_word() {
    let mut editor = Editor::new();
    editor.insert("one two three");
    editor.kill_space_word();
    assert_eq!(text(&editor), "one two ");
    assert_eq!(editor.cursor(), 8);
    editor.kill_space_word();
    assert_eq!(text(&editor), "one ");
    editor.kill_space_word();
    assert_eq!(text(&editor), "");
    assert_eq!(editor.cursor(), 0);
    editor.kill_space_word();
    assert_eq!(text(&editor), "");

    let mut editor = Editor::new();
    editor.insert("one two   ");
    editor.kill_space_word();
    assert_eq!(text(&editor), "one ", "trailing spaces go with the word");

    let mut editor = Editor::new();
    editor.insert("ab\ncd");
    editor.kill_space_word();
    assert_eq!(text(&editor), "ab\n", "a newline stops the word");
}

#[test]
fn home_and_end_are_the_ends_of_the_visual_row() {
    let mut editor = Editor::new();
    editor.insert("abcdefgh");
    editor.left();
    editor.left();
    assert_eq!(editor.cursor(), 6);
    editor.home(4);
    assert_eq!(editor.cursor(), 4, "home is the start of the wrapped row");
    editor.end(4);
    assert_eq!(editor.cursor(), 8);
    editor.home(4);
    editor.left();
    editor.home(4);
    assert_eq!(editor.cursor(), 0);
}

#[test]
fn up_and_down_move_by_visual_line() {
    let mut editor = Editor::new();
    editor.insert("abcdefgh");
    assert!(editor.up(4));
    assert_eq!(editor.cursor(), 4);
    assert!(!editor.up(4), "up on the first row is for the history");
    assert_eq!(editor.cursor(), 4);
    assert!(editor.down(4));
    assert_eq!(editor.cursor(), 8);
    assert!(!editor.down(4), "down on the last row is for the history");

    // A column past the end of the target row lands on its end.
    let mut editor = Editor::new();
    editor.insert("b\nabcdefgh");
    assert!(editor.up(8));
    assert_eq!(editor.cursor(), 1);

    // A newline ends a row, so a short last line is its own row.
    let mut editor = Editor::new();
    editor.insert("aaaaaaaaaa\nb");
    assert_eq!(editor.cursor(), 12);
    assert!(editor.up(5));
    assert_eq!(editor.cursor(), 6);
    assert!(editor.down(5));
    assert_eq!(editor.cursor(), 12);
}

#[test]
fn a_long_line_wraps_at_the_width() {
    let mut editor = Editor::new();
    editor.insert("abcdefgh");
    assert_eq!(visible(&mut editor, 4, 2), vec!["abcd", "efgh"]);

    let mut editor = Editor::new();
    editor.insert("abcde");
    assert_eq!(visible(&mut editor, 4, 2), vec!["abcd", "e"]);
}

#[test]
fn wide_characters_wrap_whole() {
    let mut editor = Editor::new();
    editor.insert("世界世界");
    assert_eq!(visible(&mut editor, 4, 2), vec!["世界", "世界"]);
    assert_eq!(visible(&mut editor, 3, 4), vec!["世", "界", "世", "界"]);

    let layout = editor.layout(4, 2);
    assert_eq!(layout.total(), 2);
    assert_eq!(layout.cursor_row(), 1);
    assert_eq!(layout.cursor_col(), 4);
}

#[test]
fn a_combining_mark_takes_no_column() {
    let mut editor = Editor::new();
    editor.insert("e\u{301}e\u{301}");
    let layout = editor.layout(10, 1);
    assert_eq!(layout.cursor_col(), 2);
    assert_eq!(visible(&mut editor, 10, 1), vec!["e\u{301}e\u{301}"]);
}

#[test]
fn new_line_starts_a_row() {
    let mut editor = Editor::new();
    editor.insert("ab");
    editor.new_line();
    editor.insert("cd");
    assert_eq!(editor.cursor(), 5);
    assert_eq!(visible(&mut editor, 10, 4), vec!["ab", "cd"]);
}

#[test]
fn layout_shows_only_the_rows_around_the_cursor() {
    let mut editor = Editor::new();
    editor.insert("a\nb\nc\nd\ne");
    {
        let layout = editor.layout(10, 3);
        assert_eq!(layout.total(), 5);
        assert_eq!(layout.first(), 2);
        assert_eq!(layout.rows().len(), 3);
        assert_eq!(layout.cursor_row(), 2);
        assert_eq!(layout.cursor_col(), 1);
    }
    assert_eq!(visible(&mut editor, 10, 3), vec!["c", "d", "e"]);

    // A smaller height than rows keeps the cursor row in view at the bottom.
    let mut editor = Editor::new();
    editor.insert("a\nb\nc");
    {
        let layout = editor.layout(10, 1);
        assert_eq!(layout.rows().len(), 1);
        assert_eq!(layout.cursor_row(), 0);
    }
    assert_eq!(visible(&mut editor, 10, 1), vec!["c"]);
}

#[test]
fn a_box_capped_at_a_third_of_the_screen_scrolls_to_the_edit_point() {
    let mut editor = Editor::new();
    for line in 0..20 {
        editor.insert(&format!("line {line}\n"));
    }
    // A 30-row terminal gives the text box seven text rows: a third of the
    // screen, less the padding rows and the footer.
    let layout = editor.layout(10, 7);
    assert_eq!(layout.total(), 21, "a trailing newline is its own row");
    assert_eq!(layout.rows().len(), 7, "the box stops at its seven rows");
    assert_eq!(layout.first(), 14, "the text scrolled to keep the cursor");
    assert_eq!(
        layout.cursor_row(),
        6,
        "the edit point is the last row shown"
    );
    assert_eq!(layout.cursor_col(), 0, "the cursor starts the empty row");
}

#[test]
fn history_up_and_down_returns_the_draft() {
    let history: Vec<Arc<str>> = vec![Arc::from("one"), Arc::from("two")];
    let mut editor = Editor::new();
    editor.insert("draft");
    assert!(editor.history_up(&history));
    assert_eq!(text(&editor), "two");
    assert!(editor.history_up(&history));
    assert_eq!(text(&editor), "one");
    assert!(!editor.history_up(&history), "the oldest entry is the last");
    assert_eq!(text(&editor), "one");
    assert!(editor.history_down(&history));
    assert_eq!(text(&editor), "two");
    assert!(editor.history_down(&history));
    assert_eq!(
        text(&editor),
        "draft",
        "down past the newest returns the draft"
    );
    assert!(!editor.history_down(&history));
    assert_eq!(text(&editor), "draft");

    let empty: [Arc<str>; 0] = [];
    let mut editor = Editor::new();
    assert!(!editor.history_up(&empty));
    assert!(!editor.history_down(&empty));
}

#[test]
fn a_100_kb_paste_inserts_as_one_text() {
    let paste = "x".repeat(100 * 1024);
    let mut editor = Editor::new();
    editor.insert(&paste);
    assert_eq!(editor.len(), 100 * 1024);
    assert_eq!(editor.cursor(), 100 * 1024);
    {
        let layout = editor.layout(80, 24);
        assert_eq!(layout.total(), 100 * 1024 / 80);
        assert_eq!(layout.cursor_row(), 23);
        assert_eq!(
            layout.cursor_col(),
            80,
            "the row is full, the cursor is past it"
        );
    }
    // One more character starts a new row.
    editor.insert("y");
    {
        let layout = editor.layout(80, 24);
        assert_eq!(layout.total(), 100 * 1024 / 80 + 1);
        assert_eq!(layout.cursor_row(), 23);
        assert_eq!(layout.cursor_col(), 1);
    }
    assert_eq!(editor.take().len(), 100 * 1024 + 1);
    assert!(editor.is_empty());
}

#[test]
fn a_paste_in_the_middle_keeps_the_text_in_order() {
    let mut editor = Editor::new();
    editor.insert("ac");
    editor.left();
    editor.insert("b");
    assert_eq!(text(&editor), "abc");
    assert_eq!(editor.cursor(), 2);
    let (first, second) = editor.chunks();
    assert_eq!(first, "ab", "the gap sits at the cursor");
    assert_eq!(second, "c");
    assert_eq!(editor.slice(1, 3), ("b", "c"));
}

#[test]
fn every_key_works_on_wide_text() {
    // 世 and 界 are three bytes and two columns each.
    let mut editor = Editor::new();
    editor.insert("世界世");
    editor.line_start();
    assert_eq!(editor.cursor(), 0);
    editor.right();
    assert_eq!(editor.cursor(), 3);
    editor.end(2);
    assert_eq!(editor.cursor(), 6, "end of a full row moves on to the next");
    assert!(editor.up(2));
    assert_eq!(editor.cursor(), 3);
    editor.home(2);
    assert_eq!(editor.cursor(), 0);
    editor.delete();
    assert_eq!(text(&editor), "界世");
    editor.backspace();
    assert_eq!(editor.cursor(), 0);
    editor.kill_to_end();
    assert_eq!(text(&editor), "");
    editor.insert("世界世");
    editor.kill_to_start();
    assert_eq!(text(&editor), "");
    editor.insert("世界世");
    assert_eq!(visible(&mut editor, 2, 3), vec!["世", "界", "世"]);
}

#[test]
fn every_key_works_on_a_combining_mark() {
    // e + U+0301 is one grapheme, three bytes and one column.
    let mut editor = Editor::new();
    editor.insert("e\u{301}e\u{301}");
    editor.line_start();
    assert_eq!(editor.cursor(), 0);
    editor.right();
    assert_eq!(editor.cursor(), 3);
    editor.end(10);
    assert_eq!(editor.cursor(), 6);
    editor.kill_to_start();
    assert_eq!(text(&editor), "");
    editor.insert("e\u{301}e\u{301}");
    editor.home(10);
    assert_eq!(editor.cursor(), 0);
    editor.delete();
    assert_eq!(text(&editor), "e\u{301}");
    editor.backspace();
    assert_eq!(editor.cursor(), 0);
    editor.kill_to_end();
    assert_eq!(text(&editor), "");
}

#[test]
fn a_variation_selector_emoji_measures_as_one_grapheme() {
    // U+2764 U+FE0F is one grapheme of two columns, the same as the cell grid
    // draws it; the editor must not count the heart and the selector apart.
    let mut editor = Editor::new();
    editor.insert("\u{2764}\u{FE0F}x");
    assert_eq!(
        editor.layout(10, 1).cursor_col(),
        3,
        "the heart takes two columns"
    );
    assert_eq!(visible(&mut editor, 10, 1), vec!["\u{2764}\u{FE0F}x"]);

    // Three hearts take six columns: two rows of five, as the grid wraps them.
    let mut editor = Editor::new();
    editor.insert("\u{2764}\u{FE0F}\u{2764}\u{FE0F}\u{2764}\u{FE0F}");
    {
        let layout = editor.layout(5, 2);
        assert_eq!(layout.total(), 2, "three two-column hearts need two rows");
        assert_eq!(layout.cursor_row(), 1);
        assert_eq!(layout.cursor_col(), 2);
    }
    assert_eq!(
        visible(&mut editor, 5, 2),
        vec!["\u{2764}\u{FE0F}\u{2764}\u{FE0F}", "\u{2764}\u{FE0F}"]
    );

    // Up goes by columns: back to just after the first heart.
    let mut editor = Editor::new();
    editor.insert("\u{2764}\u{FE0F}\u{2764}\u{FE0F}\u{2764}\u{FE0F}");
    assert!(editor.up(5));
    assert_eq!(editor.cursor(), 6);
}

#[test]
fn typing_keeps_the_room_the_buffer_grew_to() {
    let mut editor = Editor::new();
    editor.insert(&"x".repeat(4096));
    let grown = editor.gap.capacity();
    let mut last = grown;
    let mut grows = 0;
    for _ in 0..10_000 {
        editor.insert("y");
        let now = editor.gap.capacity();
        assert!(now >= last, "the capacity never shrinks");
        if now != last {
            grows += 1;
            last = now;
        }
    }
    println!("capacity {grown} -> {last} in {grows} grow(s) for 10000 keys");
    assert!(
        grows <= 4,
        "{grows} grows for 10,000 keys: the buffer grows by doubling, not by 64 bytes"
    );

    // `take` and `clear` keep the room, so the next message allocates nothing.
    editor.take();
    assert_eq!(editor.gap.capacity(), last, "take keeps the room");
    editor.insert("a");
    assert_eq!(
        editor.gap.capacity(),
        last,
        "a key after a clear allocates nothing"
    );
}

#[test]
fn a_key_in_a_large_buffer_wraps_a_few_rows() {
    // 110 KB at 80 columns is 1408 rows: a key must not re-wrap them.
    let mut editor = Editor::new();
    editor.insert(&"x".repeat(110 * 1024));
    let _ = editor.layout(80, 24);
    editor.reset_rows_wrapped();
    for _ in 0..100 {
        editor.insert("y");
        let _ = editor.layout(80, 24);
    }
    let wrapped = editor.rows_wrapped();
    println!("{wrapped} rows wrapped for 100 keys in a 110 KB buffer");
    assert!(
        wrapped <= 200,
        "{wrapped} rows for 100 keys: the wrap must follow the edit, not the buffer"
    );
}

#[test]
fn a_key_at_the_start_of_a_large_buffer_wraps_only_the_window() {
    // 110 KB at 80 columns is about 1408 rows. A key at offset 0 drops every
    // row below it, so the layout must wrap the rows the window shows, not the
    // whole buffer again.
    let mut editor = Editor::new();
    editor.insert(&"x".repeat(110 * 1024));
    let _ = editor.layout(80, 24);
    editor.line_start();
    assert_eq!(editor.cursor(), 0);
    let _ = editor.layout(80, 24);
    editor.reset_rows_wrapped();
    for _ in 0..100 {
        editor.line_start();
        editor.insert("y");
        assert_eq!(editor.layout(80, 24).cursor_row(), 0);
    }
    let wrapped = editor.rows_wrapped();
    println!("{wrapped} rows wrapped for 100 keys at offset 0 of a 110 KB buffer");
    assert!(
        wrapped <= 100 * 24,
        "{wrapped} rows for 100 keys: the layout wraps the visible window, not the buffer"
    );

    // The rows the lazy wrap built are still the right ones: a full wrap of the
    // text counts the same rows as a fresh editor wraps.
    let whole = text(&editor);
    let mut fresh = Editor::new();
    fresh.insert(&whole);
    assert_eq!(
        editor.layout(80, u16::MAX).total(),
        fresh.layout(80, u16::MAX).total()
    );
}

#[test]
fn a_tab_is_one_column_and_other_controls_none() {
    // The screen draws a tab as one space and writes nothing for another
    // control character, so the editor measures them the same way.
    let mut editor = Editor::new();
    editor.insert("a\tb\u{4e16}");
    let layout = editor.layout(10, 1);
    assert_eq!(layout.total(), 1);
    assert_eq!(
        layout.cursor_col(),
        5,
        "a, the tab, b and the two columns of 世"
    );
    assert_eq!(visible(&mut editor, 10, 1), vec!["a\tb\u{4e16}"]);
    assert_eq!(
        visible(&mut editor, 3, 2),
        vec!["a\tb", "\u{4e16}"],
        "a tab counts in the wrap like one column"
    );

    let mut editor = Editor::new();
    editor.insert("a\u{1b}b");
    let layout = editor.layout(10, 1);
    assert_eq!(layout.total(), 1);
    assert_eq!(layout.cursor_col(), 2, "ESC takes no column");
    assert_eq!(
        visible(&mut editor, 10, 1),
        vec!["a\u{1b}b"],
        "the text is kept, only the width is none"
    );
}

#[test]
fn a_grapheme_the_gap_splits_is_measured_whole() {
    // A heart and its variation selector are worth two columns together and
    // one apart; the gap between them must not change the count.
    let mut editor = Editor::new();
    editor.insert("\u{2764}\u{FE0F}");
    editor.gap.move_to(3);
    assert_eq!(editor.layout(10, 1).cursor_col(), 2);
    assert_eq!(editor.layout(10, 1).total(), 1);
}

#[test]
fn the_cached_wrap_matches_a_fresh_wrap() {
    // The rows an edit keeps are compared against a wrap from the start, at
    // the width the edits happen at; every fiftieth step the other widths are
    // checked too, and the last of them leaves the cache at the edit width.
    let mut editor = Editor::new();
    let mut seed = 0x5eedu64;
    let mut next = |n: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize % n
    };
    let keys = [
        "a",
        "b",
        "\u{4e16}",
        "\u{301}",
        "\n",
        " ",
        "\u{2764}\u{FE0F}",
    ];
    for step in 0..500 {
        match next(12) {
            0..=6 => {
                let key = keys[next(keys.len())];
                editor.insert(key);
            }
            7 => editor.backspace(),
            8 => editor.left(),
            9 => editor.right(),
            10 => editor.home(7),
            _ => editor.end(7),
        }
        let (before, after) = editor.chunks();
        let whole = format!("{before}{after}");
        let widths: &[u16] = if step % 50 == 49 { &[1, 3, 7] } else { &[7] };
        for &width in widths {
            let mut fresh = Editor::new();
            fresh.insert(&whole);
            fresh.gap.move_to(editor.cursor());
            let live = editor.layout(width, u16::MAX / 2);
            let want = fresh.layout(width, u16::MAX / 2);
            assert_eq!(
                live.total(),
                want.total(),
                "rows at width {width} for {whole:?}"
            );
            assert_eq!(
                (live.cursor_row(), live.cursor_col()),
                (want.cursor_row(), want.cursor_col()),
                "cursor at width {width} for {whole:?}"
            );
            let rows = live.rows().to_vec();
            assert_eq!(rows.len(), want.rows().len(), "visible rows for {whole:?}");
            for (row, expected) in rows.iter().zip(want.rows()) {
                assert_eq!(
                    (row.start(), row.end()),
                    (expected.start(), expected.end()),
                    "row at width {width} for {whole:?}"
                );
            }

            // A window smaller than the text wraps only the rows it shows, and
            // those are the same rows the full wrap has there.
            let height = 1 + next(6) as u16;
            let short = editor.layout(width, height);
            let first = short.first();
            let window: Vec<(usize, usize)> = short
                .rows()
                .iter()
                .map(|row| (row.start(), row.end()))
                .collect();
            assert_eq!(
                window.len(),
                usize::from(height).min(want.total()),
                "a window of {height} rows at width {width} for {whole:?}"
            );
            assert!(
                first + window.len() <= want.total(),
                "the window at width {width} for {whole:?} stays in the text"
            );
            assert_eq!(
                first + usize::from(short.cursor_row()),
                usize::from(want.cursor_row()),
                "the cursor row in the window at width {width} for {whole:?}"
            );
            assert_eq!(
                short.cursor_col(),
                want.cursor_col(),
                "the cursor column in the window at width {width} for {whole:?}"
            );
            for (i, row) in window.iter().enumerate() {
                let expected = want.rows()[first + i];
                assert_eq!(
                    *row,
                    (expected.start(), expected.end()),
                    "window row {i} at width {width} for {whole:?}"
                );
            }
        }
    }
}

#[test]
fn an_empty_editor_shows_one_empty_row() {
    let mut editor = Editor::new();
    {
        let layout = editor.layout(10, 3);
        assert_eq!(layout.total(), 1);
        assert_eq!(layout.rows().len(), 1);
        assert_eq!(layout.cursor_row(), 0);
        assert_eq!(layout.cursor_col(), 0);
    }
    assert_eq!(visible(&mut editor, 10, 3), vec![""]);
}

#[test]
fn a_trailing_newline_shows_an_empty_row() {
    let mut editor = Editor::new();
    editor.insert("ab\n");
    assert_eq!(visible(&mut editor, 10, 3), vec!["ab", ""]);
    assert_eq!(editor.layout(10, 3).cursor_row(), 1);
    editor.home(10);
    assert_eq!(editor.cursor(), 3, "home on the empty line stays put");
    editor.end(10);
    assert_eq!(editor.cursor(), 3);
}

/// A paste of `lines` lines of a compiler log, the way the text box folds one.
fn log(lines: usize) -> String {
    let mut text = String::from("error[E0308]: mismatched types");
    for line in 1..lines {
        text.push_str("\n  ");
        text.push_str(&line.to_string());
    }
    text
}

/// A folded chip is one mark in the text and a whole label on screen: the layout
/// measures the mark as the columns the chip draws, so the wrap, the row it
/// shows on and the cursor column agree with the box that paints it.
#[test]
fn a_folded_chip_is_measured_at_the_width_it_draws() {
    let mut editor = Editor::new();
    editor.paste(log(4));
    assert_eq!(
        editor.pastes()[0].width(),
        37,
        "≡ + the first line + … + the count"
    );
    let layout = editor.layout(80, 2);
    assert_eq!(
        layout.cursor_col(),
        37,
        "the cursor stands after the whole chip, not after one cell of it"
    );

    // Four cells of text and a chip of 37 do not fit a row of 40: the chip
    // starts the row below, and the cursor is past it on that row.
    let mut editor = Editor::new();
    editor.insert("abcd");
    editor.paste(log(4));
    let layout = editor.layout(40, 3);
    assert_eq!(layout.total(), 2, "the chip wraps to a row of its own");
    assert_eq!(layout.cursor_row(), 1, "the cursor is on the chip's row");
    assert_eq!(layout.cursor_col(), 37, "past the chip on its own row");
    let rows = layout.rows().to_vec();
    assert_eq!(
        (rows[0].start(), rows[0].end(), rows[1].start()),
        (0, 4, 4),
        "the text row ends where the chip's row starts"
    );
}

/// A paste whose first line is 300 columns of `x` and whose six other lines are
/// 120 columns of `y`: the first line is far wider than a chip's label may be.
fn long_first_line() -> String {
    let mut text = "x".repeat(300);
    for _ in 0..6 {
        text.push('\n');
        text.push_str(&"y".repeat(120));
    }
    text
}

/// A paste of `lines` lines whose lines are short and distinct.
fn long_rows(lines: usize) -> String {
    let mut text = String::from("line 1");
    for line in 2..=lines {
        text.push_str("\nline ");
        text.push_str(&line.to_string());
    }
    text
}

/// A chip's label is its first line cut to forty columns, so a first line wider
/// than the row cannot hide the count of the lines the chip holds back.
#[test]
fn a_chip_label_cuts_a_very_long_first_line() {
    let mut editor = Editor::new();
    let text = long_first_line();
    editor.paste(text.clone());
    let chip = &editor.pastes()[0];
    assert_eq!(chip.other(), 6, "six lines behind the first");
    assert!(
        chip.width() <= 49,
        "the label stays narrow enough to show its count: {}",
        chip.width()
    );
    assert_eq!(
        UnicodeWidthStr::width(chip.first_line()),
        40,
        "the first line is cut at forty columns"
    );
    assert_eq!(editor.take(), text, "the send path still gets every byte");
}

/// A cut of the first line lands on a char boundary: a wide character is never
/// left in half.
#[test]
fn a_chip_label_never_cuts_a_wide_char_in_half() {
    let mut editor = Editor::new();
    let text = format!("{}\n2\n3\n4", "界".repeat(300));
    editor.paste(text);
    let chip = &editor.pastes()[0];
    let first = chip.first_line();
    assert!(
        first.chars().all(|c| c == '界'),
        "the cut leaves whole characters: {first:?}"
    );
    assert_eq!(
        UnicodeWidthStr::width(first),
        40,
        "forty columns of a two-column character"
    );
}

/// A fold cuts the first line again: an edit that widened it cannot hide the
/// count either, and the chip keeps the whole text.
#[test]
fn a_chip_label_cuts_the_first_line_when_it_folds() {
    let mut editor = Editor::new();
    editor.paste(format!("{}\n2\n3\n4", "z".repeat(60)));
    assert_eq!(
        UnicodeWidthStr::width(editor.pastes()[0].first_line()),
        40,
        "the paste cut the first line"
    );
    assert!(editor.toggle_paste(), "tab opens the chip");
    editor.right();
    editor.insert(&"w".repeat(20));
    assert!(editor.toggle_paste(), "tab folds it back");
    assert_eq!(
        UnicodeWidthStr::width(editor.pastes()[0].first_line()),
        40,
        "the fold cut the wider first line"
    );
    assert_eq!(
        editor.pastes()[0].text().len(),
        20 + 60 + 6,
        "the chip still holds the whole paste"
    );
}

/// Tab after the chip, on other text, folds the open chip back: the edit point
/// moves with the text that follows the mark, so the next key lands there.
#[test]
fn a_chip_folds_from_the_cursor_after_it() {
    let mut editor = Editor::new();
    let pasted = long_rows(4);
    editor.insert("head ");
    editor.paste(pasted.clone());
    editor.insert(" tail");
    let (_, end) = editor.pastes()[0].range();
    while editor.cursor() > end {
        editor.left();
    }
    assert_eq!(editor.cursor(), end, "the edit point is at the chip's end");
    assert!(editor.toggle_paste(), "tab opens the chip");
    let (_, end) = editor.pastes()[0].range();
    while editor.cursor() < end {
        editor.right();
    }
    editor.right();
    assert_eq!(editor.cursor(), end + 1, "the edit point is past the chip");
    assert!(
        editor.toggle_paste(),
        "tab with the edit point off the chip folds it"
    );
    assert!(!editor.pastes()[0].is_open(), "the chip is folded");
    editor.insert("!");
    assert_eq!(
        editor.take(),
        format!("head {} !tail", pasted),
        "the key after the fold lands after the mark"
    );
}

/// A chip folds at its index, as a click on one of its rows does, and its text
/// is the same afterwards.
#[test]
fn a_chip_folds_at_any_index_of_an_open_one() {
    let mut editor = Editor::new();
    let pasted = long_rows(4);
    editor.paste(pasted.clone());
    assert!(editor.toggle_paste(), "tab opens the chip");
    assert!(editor.pastes()[0].is_open(), "the chip is open");
    assert!(editor.fold_paste(0), "the chip at index zero folds");
    assert!(!editor.pastes()[0].is_open(), "the chip is folded");
    assert!(!editor.fold_paste(0), "a folded chip folds no more");
    assert!(!editor.fold_paste(3), "no chip is at that index");
    assert_eq!(editor.pastes()[0].text(), pasted, "the text is the same");
    assert_eq!(editor.take(), pasted, "and the send path gets it whole");
}

/// The text with `|` at the cursor, as the probe of a real readline prints it.
fn marked(editor: &Editor) -> String {
    let (before, after) = editor.chunks();
    let mut out = String::with_capacity(before.len() + after.len() + 1);
    out.push_str(before);
    out.push('|');
    out.push_str(after);
    out
}

/// The text typed one character at a time, as the keyboard sends it.
fn type_text(editor: &mut Editor, text: &str) {
    for c in text.chars() {
        editor.type_char(c);
    }
}

/// `count` words, `w1` up to `w<count>`, spaced by one blank.
fn numbered_words(count: usize) -> Vec<String> {
    (1..=count).map(|n| format!("w{n}")).collect()
}

/// Alt+Backspace pressed `presses` times, one hundred milliseconds apart from
/// `start`; answers the instant of the last press.
fn alt_backspace(editor: &mut Editor, start: Instant, presses: u32) -> Instant {
    for press in 0..presses {
        editor.backward_kill_word_at(start + Duration::from_millis(100 * u64::from(press)));
    }
    start + Duration::from_millis(100 * u64::from(presses.saturating_sub(1)))
}

#[test]
fn readline_move_word_back_and_forward_stop_at_the_word_edges() {
    let mut editor = Editor::new();
    editor.insert("cargo test --all");
    editor.backward_word();
    assert_eq!(marked(&editor), "cargo test --|all");
    editor.backward_word();
    assert_eq!(marked(&editor), "cargo |test --all", "the word before it");
    editor.forward_word();
    assert_eq!(marked(&editor), "cargo test| --all", "right after the word");
}

#[test]
fn readline_move_word_moves_keep_their_place_at_the_ends() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two");
    editor.forward_word();
    assert_eq!(marked(&editor), "one two|", "forward at the end stays");
    editor.backward_word();
    editor.backward_word();
    editor.backward_word();
    assert_eq!(marked(&editor), "|one two", "backward at the start stays");
}

#[test]
fn readline_move_transpose_chars_swaps_the_two_before_the_end() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab");
    editor.transpose_chars();
    assert_eq!(marked(&editor), "ba|");
}

#[test]
fn readline_move_transpose_chars_swaps_across_the_cursor() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcd");
    editor.left();
    editor.left();
    editor.transpose_chars();
    assert_eq!(marked(&editor), "acb|d", "the cursor moves past the swap");
}

#[test]
fn readline_move_transpose_chars_at_the_start_does_nothing() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abc");
    editor.line_start();
    editor.transpose_chars();
    assert_eq!(marked(&editor), "|abc");
    let mut editor = Editor::new();
    type_text(&mut editor, "a");
    editor.transpose_chars();
    assert_eq!(marked(&editor), "a|", "one character has nothing to swap");
}

#[test]
fn readline_move_case_and_transpose_make_hello_world() {
    let mut editor = Editor::new();
    type_text(&mut editor, "hello wrold");
    for _ in 0..3 {
        editor.left();
    }
    editor.transpose_chars();
    assert_eq!(marked(&editor), "hello wor|ld");
    editor.backward_word();
    editor.upcase_word();
    assert_eq!(marked(&editor), "hello WORLD|");
}

#[test]
fn readline_move_case_commands_change_one_word() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two");
    editor.line_start();
    editor.capitalize_word();
    assert_eq!(marked(&editor), "One| two");

    let mut editor = Editor::new();
    type_text(&mut editor, "ONE TWO");
    editor.line_start();
    editor.downcase_word();
    assert_eq!(marked(&editor), "one| TWO");

    editor.line_end();
    editor.downcase_word();
    assert_eq!(marked(&editor), "one TWO|", "no word after the end");
}

#[test]
fn readline_move_delete_horizontal_space_joins_the_words() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one   two");
    for _ in 0..4 {
        editor.left();
    }
    assert_eq!(marked(&editor), "one  | two");
    editor.delete_horizontal_space();
    assert_eq!(marked(&editor), "one|two");
}

#[test]
fn readline_move_transpose_words_swaps_the_last_two() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two");
    editor.transpose_words();
    assert_eq!(marked(&editor), "two one|");
}

#[test]
fn readline_move_transpose_words_swaps_the_word_before_the_cursor() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two three four");
    for _ in 0..6 {
        editor.left();
    }
    editor.transpose_words();
    assert_eq!(marked(&editor), "one three two| four");
}

#[test]
fn readline_move_transpose_words_needs_two_words() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one");
    editor.transpose_words();
    assert_eq!(marked(&editor), "one|");
    editor.line_start();
    editor.transpose_words();
    assert_eq!(marked(&editor), "|one");
}

#[test]
fn readline_move_char_search_moves_to_the_character() {
    let mut editor = Editor::new();
    type_text(&mut editor, "a.b.c");
    editor.line_start();
    editor.search_char(true);
    editor.next_key(Some('.'));
    assert_eq!(marked(&editor), "a|.b.c");

    editor.search_char(true);
    editor.next_key(Some('.'));
    assert_eq!(marked(&editor), "a.b|.c");

    editor.search_char(false);
    editor.next_key(Some('.'));
    assert_eq!(marked(&editor), "a|.b.c", "backward finds the one before");
}

#[test]
fn readline_move_char_search_does_not_wrap() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abc.");
    editor.line_start();
    editor.search_char(true);
    editor.next_key(Some('.'));
    assert_eq!(marked(&editor), "abc|.");
    editor.search_char(true);
    editor.next_key(Some('.'));
    assert_eq!(
        marked(&editor),
        "abc|.",
        "the search stops at the end, it does not wrap"
    );
}

#[test]
fn readline_move_char_search_with_no_character_settles() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abc");
    editor.search_char(false);
    assert!(editor.wants_key(), "the search waits for its character");
    editor.next_key(None);
    assert!(!editor.wants_key(), "a key that is no character ends it");
    assert_eq!(marked(&editor), "abc|");
}

#[test]
fn readline_move_exchange_point_and_mark_returns_to_the_mark() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abc");
    editor.set_mark();
    editor.line_start();
    assert_eq!(marked(&editor), "|abc");
    editor.exchange_point_and_mark();
    assert_eq!(marked(&editor), "abc|", "the cursor goes back to the mark");
    editor.exchange_point_and_mark();
    assert_eq!(marked(&editor), "|abc", "and back again");
}

#[test]
fn readline_move_exchange_with_no_mark_keeps_the_cursor() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abc");
    editor.exchange_point_and_mark();
    assert_eq!(marked(&editor), "abc|");
}

#[test]
fn readline_move_forward_and_backward_char_take_the_argument() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcdef");
    editor.line_start();
    editor.forward_char();
    assert_eq!(marked(&editor), "a|bcdef");
    editor.backward_char();
    assert_eq!(marked(&editor), "|abcdef");
}

#[test]
fn readline_move_delete_char_removes_the_one_after_the_cursor() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcd");
    editor.line_start();
    editor.delete_char();
    assert_eq!(marked(&editor), "|bcd");
    editor.backward_delete_char();
    assert_eq!(marked(&editor), "|bcd", "nothing before the cursor");
}

#[test]
fn readline_move_digit_arguments_repeat_the_next_command() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcdef");
    editor.digit(3);
    editor.backward_delete_char();
    assert_eq!(marked(&editor), "abc|");

    editor.digit(0);
    editor.backward_delete_char();
    assert_eq!(marked(&editor), "abc|", "a zero argument runs it no time");

    editor.digit(1);
    editor.digit(2);
    editor.backward_delete_char();
    assert_eq!(marked(&editor), "|", "digits build one number");
}

#[test]
fn readline_arg_negative_reverses_the_command() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcdef");
    editor.negative_argument();
    editor.forward_char();
    assert_eq!(marked(&editor), "abcde|f", "a forward move goes backward");

    let mut editor = Editor::new();
    type_text(&mut editor, "abcdef");
    editor.negative_argument();
    editor.digit(2);
    editor.backward_delete_char();
    assert_eq!(
        marked(&editor),
        "abcdef|",
        "a delete at the end takes nothing"
    );

    let mut editor = Editor::new();
    type_text(&mut editor, "foo bar");
    editor.negative_argument();
    editor.kill_space_word();
    assert_eq!(
        marked(&editor),
        "foo |",
        "a backward argument kills backward"
    );
}

#[test]
fn readline_arg_repeats_a_word_move_and_a_typed_character() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two three");
    editor.line_start();
    editor.digit(2);
    editor.forward_word();
    assert_eq!(marked(&editor), "one two| three");

    editor.digit(3);
    editor.type_char('z');
    assert_eq!(marked(&editor), "one twozzz| three");
}

#[test]
fn readline_arg_case_commands_take_the_count() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two three");
    editor.line_start();
    editor.digit(2);
    editor.upcase_word();
    assert_eq!(marked(&editor), "ONE TWO| three");
}

#[test]
fn readline_kill_word_forward_skips_the_gap_before_the_word() {
    let mut editor = Editor::new();
    type_text(&mut editor, "foo bar baz");
    editor.line_start();
    editor.forward_word();
    editor.kill_word();
    assert_eq!(marked(&editor), "foo| baz", "the gap and the word go");
}

#[test]
fn readline_kill_backward_takes_the_alphanumeric_run() {
    let mut editor = Editor::new();
    type_text(&mut editor, "foo-bar");
    editor.backward_kill_word();
    assert_eq!(marked(&editor), "foo-|", "the run, not the whitespace");
}

#[test]
fn readline_kill_burst_eighteen_presses_take_thirty_two_words() {
    let mut editor = Editor::new();
    let words = numbered_words(40);
    type_text(&mut editor, &words.join(" "));
    alt_backspace(&mut editor, Instant::now(), 18);
    assert_eq!(
        text(&editor).trim_end(),
        words[..8].join(" "),
        "the first eight presses kill a word each, the next eight two, the last two four"
    );
}

#[test]
fn readline_kill_burst_a_gap_over_the_limit_starts_a_new_burst() {
    let mut editor = Editor::new();
    let words = numbered_words(40);
    type_text(&mut editor, &words.join(" "));
    let last = alt_backspace(&mut editor, Instant::now(), 17);
    assert_eq!(
        text(&editor).trim_end(),
        words[..12].join(" "),
        "sixteen presses kill twenty four words and the seventeenth four"
    );
    editor.backward_kill_word_at(last + Duration::from_millis(201));
    assert_eq!(
        text(&editor).trim_end(),
        words[..11].join(" "),
        "a gap over the limit makes the press the first of a burst"
    );
}

#[test]
fn readline_kill_burst_a_gap_of_the_limit_continues_the_burst() {
    let mut editor = Editor::new();
    let words = numbered_words(40);
    type_text(&mut editor, &words.join(" "));
    let last = alt_backspace(&mut editor, Instant::now(), 17);
    editor.backward_kill_word_at(last + Duration::from_millis(200));
    assert_eq!(
        text(&editor).trim_end(),
        words[..8].join(" "),
        "a press on the limit is the eighteenth of the burst, and kills four"
    );
}

#[test]
fn readline_kill_burst_another_key_ends_it() {
    let mut editor = Editor::new();
    let words = numbered_words(40);
    type_text(&mut editor, &words.join(" "));
    let last = alt_backspace(&mut editor, Instant::now(), 17);
    editor.set_mark();
    editor.backward_kill_word_at(last + Duration::from_millis(100));
    assert_eq!(
        text(&editor).trim_end(),
        words[..11].join(" "),
        "a key between the presses, even one that moves nothing, kills one word next"
    );
}

#[test]
fn readline_kill_burst_an_argument_kills_its_count_and_ends_the_burst() {
    let mut editor = Editor::new();
    let words = numbered_words(40);
    type_text(&mut editor, &words.join(" "));
    let last = alt_backspace(&mut editor, Instant::now(), 17);
    editor.digit(3);
    editor.backward_kill_word_at(last + Duration::from_millis(100));
    assert_eq!(
        text(&editor).trim_end(),
        words[..9].join(" "),
        "the digit key makes the press kill its own count"
    );
    editor.backward_kill_word_at(last + Duration::from_millis(200));
    assert_eq!(
        text(&editor).trim_end(),
        words[..8].join(" "),
        "the press after the argument starts a burst"
    );
}

#[test]
fn readline_kill_burst_one_undo_puts_the_burst_back() {
    let mut editor = Editor::new();
    let words = numbered_words(40);
    type_text(&mut editor, &words.join(" "));
    let before = marked(&editor);
    alt_backspace(&mut editor, Instant::now(), 18);
    assert_eq!(text(&editor).trim_end(), words[..8].join(" "));
    editor.undo();
    assert_eq!(
        marked(&editor),
        before,
        "one undo puts the thirty two words back with the cursor where it was"
    );
}

#[test]
fn readline_kill_burst_a_yank_puts_every_word_back() {
    let mut editor = Editor::new();
    let words = numbered_words(40);
    type_text(&mut editor, &words.join(" "));
    alt_backspace(&mut editor, Instant::now(), 18);
    editor.yank();
    assert_eq!(
        text(&editor),
        words.join(" "),
        "all thirty two words come back in order, as one kill"
    );
    // A kill after a pause in the same run of kills still joins the entry.
    let mut editor = Editor::new();
    type_text(&mut editor, &words.join(" "));
    let last = alt_backspace(&mut editor, Instant::now(), 2);
    editor.backward_kill_word_at(last + Duration::from_millis(500));
    editor.yank();
    assert_eq!(
        text(&editor),
        words.join(" "),
        "the kill after the pause joins the two before it"
    );
}

#[test]
fn readline_kill_burst_a_press_past_the_last_word_empties_the_box() {
    let mut editor = Editor::new();
    let words = numbered_words(29);
    type_text(&mut editor, &words.join(" "));
    alt_backspace(&mut editor, Instant::now(), 18);
    assert_eq!(
        text(&editor).trim(),
        "",
        "the eighteenth press may kill four words and three are left"
    );
    assert_eq!(editor.cursor(), 0);
}

#[test]
fn readline_kill_burst_on_an_empty_box_does_nothing() {
    let mut editor = Editor::new();
    alt_backspace(&mut editor, Instant::now(), 18);
    assert!(editor.is_empty());
    assert_eq!(editor.cursor(), 0);
    editor.yank();
    assert!(editor.is_empty(), "no kill went onto the ring");
    editor.undo();
    assert!(editor.is_empty(), "no step went into the undo log");
}

#[test]
fn readline_kill_burst_takes_a_folded_chip_whole_and_one_undo_puts_it_back() {
    let mut editor = Editor::new();
    let log = log(142);
    type_text(&mut editor, "one two ");
    editor.paste(log.clone());
    let before = marked(&editor);
    alt_backspace(&mut editor, Instant::now(), 8);
    assert!(editor.pastes().is_empty(), "the kill covered the mark");
    assert_eq!(text(&editor).trim(), "", "the words and the chip went");
    editor.undo();
    assert_eq!(
        marked(&editor),
        before,
        "one undo puts the chip and its words back"
    );
    assert_eq!(editor.pastes().len(), 1, "the chip is whole");
    assert_eq!(editor.take(), format!("one two {log}"), "with its bytes");
}

#[test]
fn readline_kill_kills_in_a_row_join_and_yank_puts_them_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two three");
    editor.unix_word_rubout();
    assert_eq!(marked(&editor), "one two |");
    editor.unix_word_rubout();
    assert_eq!(marked(&editor), "one |");
    editor.yank();
    assert_eq!(marked(&editor), "one two three|", "the kills joined");
}

#[test]
fn readline_kill_a_move_between_them_keeps_two_entries() {
    let mut editor = Editor::new();
    type_text(&mut editor, "x a b y");
    editor.unix_word_rubout();
    editor.backward_char();
    editor.unix_word_rubout();
    editor.yank();
    assert_eq!(marked(&editor), "x a b| ", "the newest kill comes back");
    editor.yank_pop();
    assert_eq!(marked(&editor), "x a y| ", "the one before it after that");
}

#[test]
fn readline_kill_yank_puts_the_newest_kill_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab cd");
    editor.unix_line_discard();
    assert_eq!(marked(&editor), "|");
    editor.yank();
    assert_eq!(marked(&editor), "ab cd|");
}

#[test]
fn readline_kill_yank_pop_without_a_yank_does_nothing() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab");
    editor.unix_word_rubout();
    editor.yank_pop();
    assert_eq!(marked(&editor), "|", "a yank-pop needs a yank before it");
}

#[test]
fn readline_kill_ctrl_k_and_ctrl_u_land_on_the_ring() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two");
    editor.line_start();
    editor.kill_line();
    assert_eq!(marked(&editor), "|");
    editor.line_end();
    editor.unix_line_discard();
    editor.yank();
    assert_eq!(marked(&editor), "one two|", "the first kill is still there");
}

#[test]
fn readline_kill_ring_keeps_the_ten_newest_kills() {
    let mut editor = Editor::new();
    for c in "abcdefghijk".chars() {
        editor.insert(&c.to_string());
        editor.unix_word_rubout();
        assert!(editor.is_empty(), "each kill takes its one character");
    }
    editor.yank();
    let mut seen = vec![text(&editor)];
    for _ in 0..10 {
        editor.yank_pop();
        seen.push(text(&editor));
    }
    let want: Vec<String> = "kjihgfedcbk".chars().map(String::from).collect();
    assert_eq!(
        seen, want,
        "the eleventh kill dropped the oldest, and the ring wraps"
    );
}

#[test]
fn readline_undo_takes_back_a_run_of_typed_characters() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab cd");
    editor.undo();
    assert_eq!(
        marked(&editor),
        "|",
        "characters typed in a row are one step"
    );
}

#[test]
fn readline_undo_puts_the_cursor_where_the_group_started() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcd");
    editor.left();
    editor.left();
    editor.type_char('X');
    editor.undo();
    assert_eq!(marked(&editor), "ab|cd");
}

#[test]
fn readline_undo_starts_a_new_group_after_a_move() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab");
    editor.line_start();
    type_text(&mut editor, "cd");
    assert_eq!(marked(&editor), "cd|ab");
    editor.undo();
    assert_eq!(marked(&editor), "|ab");
    editor.undo();
    assert_eq!(marked(&editor), "|", "and the first group before it");
}

#[test]
fn readline_undo_restores_a_kill_at_the_end_of_its_text() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcd");
    editor.left();
    editor.left();
    editor.kill_line();
    assert_eq!(marked(&editor), "ab|");
    editor.undo();
    assert_eq!(
        marked(&editor),
        "abcd|",
        "the cursor goes past what came back"
    );
}

#[test]
fn readline_undo_of_a_backspace_leaves_the_cursor_after_the_text() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcdef");
    for _ in 0..3 {
        editor.left();
    }
    editor.backspace();
    assert_eq!(marked(&editor), "ab|def");
    editor.undo();
    assert_eq!(marked(&editor), "abc|def");
}

#[test]
fn readline_undo_a_paste_chip_in_one_step() {
    let mut editor = Editor::new();
    editor.paste(log(142));
    assert_eq!(editor.pastes().len(), 1);
    editor.undo();
    assert!(editor.is_empty(), "the chip goes whole");
    assert!(editor.pastes().is_empty(), "and holds nothing back");
}

#[test]
fn readline_undo_puts_a_deleted_chip_back() {
    let mut editor = Editor::new();
    let log = log(142);
    editor.paste(log.clone());
    editor.backspace();
    assert!(editor.pastes().is_empty());
    editor.undo();
    assert_eq!(editor.pastes().len(), 1, "the chip is back");
    assert_eq!(editor.take(), log, "with the bytes it held");
}

#[test]
fn readline_undo_of_a_yank_takes_the_text_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab cd");
    editor.unix_word_rubout();
    editor.yank();
    assert_eq!(marked(&editor), "ab cd|");
    editor.undo();
    assert_eq!(marked(&editor), "ab |");
}

#[test]
fn readline_undo_with_nothing_to_take_back_does_nothing() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab");
    editor.undo();
    editor.undo();
    assert_eq!(marked(&editor), "|");
}

#[test]
fn readline_undo_of_a_transpose_puts_the_words_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two");
    editor.transpose_words();
    assert_eq!(marked(&editor), "two one|");
    editor.undo();
    assert_eq!(marked(&editor), "one two|");
}

#[test]
fn readline_undo_of_a_transpose_puts_the_cursor_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcd");
    editor.backward_char();
    editor.backward_char();
    assert_eq!(marked(&editor), "ab|cd");
    editor.transpose_chars();
    assert_eq!(marked(&editor), "acb|d");
    editor.undo();
    assert_eq!(marked(&editor), "ab|cd", "the cursor goes back to the swap");

    let mut editor = Editor::new();
    type_text(&mut editor, "one two three four");
    for _ in 0..6 {
        editor.backward_char();
    }
    assert_eq!(marked(&editor), "one two thre|e four");
    editor.transpose_words();
    editor.undo();
    assert_eq!(
        marked(&editor),
        "one two three| four",
        "bash puts the cursor at the end of the swapped words"
    );
}

#[test]
fn readline_undo_ends_the_run_when_the_cursor_moved() {
    let mut editor = Editor::new();
    editor.insert("wxyz");
    editor.line_start();
    type_text(&mut editor, "ab");
    editor.end(80);
    editor.type_char('c');
    editor.undo();
    assert_eq!(
        marked(&editor),
        "abwxyz|",
        "the run that End ended is the one step of its own"
    );

    let mut editor = Editor::new();
    type_text(&mut editor, "abc");
    editor.home(80);
    editor.type_char('X');
    assert_eq!(marked(&editor), "X|abc");
    editor.undo();
    assert_eq!(marked(&editor), "|abc", "Home ends the run as Ctrl+A does");
}

#[test]
fn readline_move_exchange_point_and_mark_lands_on_a_boundary() {
    let mut editor = Editor::new();
    editor.insert("aé");
    editor.set_mark();
    editor.line_start();
    type_text(&mut editor, "xy");
    editor.exchange_point_and_mark();
    assert_eq!(
        marked(&editor),
        "xyaé|",
        "the mark moved with the typed characters"
    );
    type_text(&mut editor, "z");
    assert_eq!(
        marked(&editor),
        "xyaéz|",
        "typing after the exchange stays on a boundary"
    );
}

#[test]
fn readline_undo_moves_the_mark_with_the_group_it_takes_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab");
    editor.line_start();
    type_text(&mut editor, "é");
    editor.set_mark();
    editor.line_start();
    type_text(&mut editor, "xy");
    assert_eq!(marked(&editor), "xy|éab");
    editor.undo();
    assert_eq!(marked(&editor), "|éab");
    editor.exchange_point_and_mark();
    assert_eq!(
        marked(&editor),
        "é|ab",
        "the mark went back with the text the undo took out"
    );
}

#[test]
fn readline_kill_yank_puts_a_killed_chip_back() {
    let mut editor = Editor::new();
    let log = log(10);
    editor.insert("a ");
    editor.paste(log.clone());
    editor.line_start();
    editor.kill_line();
    assert!(
        editor.pastes().is_empty(),
        "a folded chip goes into the ring whole"
    );
    editor.yank();
    assert_eq!(editor.pastes().len(), 1, "the yank puts the chip back");
    assert_eq!(editor.take(), format!("a {log}"), "with its bytes in place");
}

#[test]
fn readline_move_a_transpose_at_a_chip_leaves_it_whole() {
    let mut editor = Editor::new();
    let log = log(10);
    editor.insert("a");
    editor.paste(log.clone());
    editor.transpose_chars();
    assert_eq!(editor.pastes().len(), 1, "the transpose keeps the chip");
    assert_eq!(editor.take(), format!("a{log}"), "and its bytes in place");
}

#[test]
fn readline_kill_joining_replaces_the_front_entry() {
    let mut editor = Editor::new();
    type_text(&mut editor, "one two three");
    editor.unix_word_rubout();
    editor.unix_word_rubout();
    editor.yank();
    assert_eq!(marked(&editor), "one two three|");
    editor.yank_pop();
    assert_eq!(
        marked(&editor),
        "one two three|",
        "the joined kills are one entry, so the pop wraps to it"
    );
}

#[test]
fn readline_arg_negative_then_a_digit_is_minus_that_digit() {
    let mut editor = Editor::new();
    type_text(&mut editor, "abcdef");
    editor.negative_argument();
    editor.digit(5);
    editor.forward_char();
    assert_eq!(
        marked(&editor),
        "a|bcdef",
        "the sign holds for the digit that follows it"
    );
}

#[test]
fn readline_arg_a_long_run_of_digits_stops_at_the_cap() {
    let mut editor = Editor::new();
    type_text(&mut editor, "ab");
    for _ in 0..20 {
        editor.digit(9);
    }
    assert_eq!(
        editor.argument(),
        1_000_000,
        "the argument stops at its cap"
    );
}

#[test]
fn readline_history_search_show_drops_the_rows_of_the_text_it_replaced() {
    let mut editor = Editor::new();
    type_text(&mut editor, "aaaa bbbb cccc dddd eeee ffff");
    assert_eq!(visible(&mut editor, 10, 8).len(), 3, "the old text");
    let history = ["x".repeat(52)];
    editor.search_history(false, &history);
    assert_eq!(
        visible(&mut editor, 10, 8),
        [
            "xxxxxxxxxx",
            "xxxxxxxxxx",
            "xxxxxxxxxx",
            "xxxxxxxxxx",
            "xxxxxxxxxx",
            "xx",
        ],
        "the rows are the entry's, not the old text's"
    );
}

#[test]
fn readline_history_search_show_leaves_the_rows_on_a_boundary() {
    let mut editor = Editor::new();
    type_text(&mut editor, "aaaa bbbb cccc dddd eeee ffff");
    visible(&mut editor, 10, 8);
    let history = [format!("{} z", "é".repeat(29))];
    editor.search_history(false, &history);
    editor.search_push('z', &history);
    let rows = visible(&mut editor, 10, 8);
    assert!(
        rows.last().is_some_and(|row| row.ends_with('z')),
        "{rows:?}"
    );
}

#[test]
fn readline_history_search_abort_gives_the_walk_its_draft_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "my draft");
    let history = ["old one", "old two"];
    assert!(editor.history_up(&history));
    assert_eq!(text(&editor), "old two");
    editor.search_history(false, &history);
    editor.search_abort();
    assert_eq!(text(&editor), "old two", "the search gave the entry back");
    assert!(editor.history_down(&history));
    assert_eq!(
        text(&editor),
        "my draft",
        "the draft the walk kept is still there"
    );
}

#[test]
fn readline_history_search_end_then_the_newest_gives_the_draft_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "my draft");
    let history = ["old one", "old two"];
    assert!(editor.history_up(&history));
    editor.search_history(false, &history);
    editor.search_end();
    assert!(editor.history_newest());
    assert_eq!(text(&editor), "my draft", "Alt+> is the draft");
}

#[test]
fn readline_history_alt_period_after_a_send_takes_the_newest_again() {
    let mut editor = Editor::new();
    let history = ["one aaa", "two bbb", "three ccc"];
    editor.yank_last_arg(&history);
    assert_eq!(text(&editor), "ccc");
    editor.take();
    editor.yank_last_arg(&history);
    assert_eq!(text(&editor), "ccc", "an empty box takes the newest word");
}

#[test]
fn readline_history_alt_period_after_a_walk_inserts_at_the_cursor() {
    let mut editor = Editor::new();
    let history = ["one aaa", "two bbb", "three ccc"];
    editor.yank_last_arg(&history);
    assert_eq!(text(&editor), "ccc");
    assert!(editor.history_up(&history));
    assert_eq!(text(&editor), "three ccc");
    editor.yank_last_arg(&history);
    assert_eq!(
        text(&editor),
        "three cccccc",
        "the walk ends the walk of Alt+. and the word lands at the cursor"
    );
}

#[test]
fn readline_history_search_back_walks_the_matches_of_one_entry() {
    let mut editor = Editor::new();
    let history = ["ab ab ab", "zzz"];
    editor.search_history(false, &history);
    editor.search_push('a', &history);
    editor.search_push('b', &history);
    assert_eq!(text(&editor), "ab ab ab");
    assert_eq!(editor.cursor(), 6);
    editor.search_history(false, &history);
    assert_eq!(editor.cursor(), 3, "the match before it");
    editor.search_history(false, &history);
    assert_eq!(editor.cursor(), 0, "the first match");
    editor.search_history(false, &history);
    assert_eq!(editor.cursor(), 0, "no match left");
    assert!(
        editor
            .search_prompt()
            .is_some_and(|prompt| prompt.starts_with("(failed reverse-i-search)")),
        "{:?}",
        editor.search_prompt()
    );
}

#[test]
fn readline_history_search_forward_walks_the_matches_of_one_entry() {
    let mut editor = Editor::new();
    let history = ["ab ab ab", "zzz"];
    editor.search_history(false, &history);
    editor.search_push('a', &history);
    editor.search_push('b', &history);
    editor.search_history(false, &history);
    editor.search_history(false, &history);
    assert_eq!(editor.cursor(), 0);
    editor.search_history(true, &history);
    assert_eq!(editor.cursor(), 3, "the match after it");
    editor.search_history(true, &history);
    assert_eq!(editor.cursor(), 6, "and the one after that");
}

#[test]
fn chip_folds_two_chips_keep_their_text() {
    let mut editor = Editor::new();
    editor.paste("a1\na2\na3\na4\na5".to_owned());
    editor.insert(" mid ");
    editor.paste("b1\nb2\nb3\nb4\nb5".to_owned());
    let want = "a1\na2\na3\na4\na5 mid b1\nb2\nb3\nb4\nb5";
    while editor.cursor() > editor.pastes()[0].range().0 {
        editor.left();
    }
    assert!(editor.toggle_paste());
    let mut out = String::new();
    editor.write_text(&mut out);
    assert_eq!(out, want, "after open");
    assert!(editor.toggle_paste());
    out.clear();
    editor.write_text(&mut out);
    assert_eq!(out, want, "after fold");
}

#[test]
fn drawn_row_down_leaves_a_blank_row_when_the_rows_are_rebuilt() {
    let mut editor = Editor::new();
    editor.insert("a\n\nb");
    assert!(editor.up(8));
    assert_eq!(editor.cursor(), 2);
    assert!(editor.down(9), "a new width rebuilds the rows");
    assert_eq!(editor.cursor(), 3);
}

#[test]
fn drawn_row_up_and_down_move_one_row_from_column_zero() {
    let mut editor = Editor::new();
    editor.insert("aaaaaaaaaa\n\nbbbbbbbbbb");
    while editor.up(4) {}
    editor.line_start();
    assert_eq!(editor.layout(4, 100).cursor_row(), 0);
    for row in 1..=6 {
        assert!(editor.down(4), "row {row}");
        assert_eq!(editor.layout(4, 100).cursor_row(), row);
    }
    assert!(!editor.down(4), "the last row is for the history");
    for row in (0..=5).rev() {
        assert!(editor.up(4), "row {row}");
        assert_eq!(editor.layout(4, 100).cursor_row(), row);
    }
    assert!(!editor.up(4), "the first row is for the history");
}

/// An image chip is a label of its number: `[Image #1]` columns, no first line
/// and no count of the lines a paste holds back.
#[test]
fn image_paste_chip_draws_its_label_and_its_width() {
    let mut editor = Editor::new();
    editor.paste_image("/p/image-1.png", 1);
    let chip = &editor.pastes()[0];
    assert_eq!(chip.image(), Some(1), "the chip holds its number");
    assert_eq!(
        chip.width(),
        UnicodeWidthStr::width("[Image #1]") as u16,
        "the label is the whole chip"
    );
    assert_eq!(chip.width(), 10, "ten columns");
    assert!(!chip.is_open(), "an image chip starts folded");
    assert_eq!(
        chip.text(),
        " @\"/p/image-1.png\"",
        "the word the worker reads"
    );
    assert_eq!(
        text(&editor),
        format!("{MARK} "),
        "the mark, and the space after it for the next key"
    );
    assert_eq!(
        editor.cursor(),
        MARK_BYTES + 1,
        "past the mark and the space"
    );

    let mut wide = Editor::new();
    wide.paste_image("/p/image-12.png", 12);
    assert_eq!(
        wide.pastes()[0].width(),
        UnicodeWidthStr::width("[Image #12]") as u16,
        "a second digit widens the label"
    );
}

/// The layout measures the mark as the label's columns, so the text before the
/// chip and the text after it stand where the box draws them.
#[test]
fn image_paste_chip_keeps_the_row_layout_around_it() {
    let mut editor = Editor::new();
    editor.insert("look ");
    editor.paste_image("/p/image-1.png", 1);
    editor.insert("now");
    let layout = editor.layout(80, 4);
    assert_eq!(layout.total(), 1, "the three parts share one row");
    assert_eq!(
        layout.cursor_col(),
        5 + 10 + 1 + 3,
        "the text before, the label, the space after it, and the text after"
    );
    let rows = layout.rows().to_vec();
    assert_eq!(
        (rows[0].start(), rows[0].end()),
        (0, 5 + MARK_BYTES + 1 + 3)
    );
}

/// The send path writes the chip's word where its mark stands, with the space
/// in front of it, so the word counts after text that ends in no space.
#[test]
fn image_paste_chip_sends_its_word_with_the_leading_space() {
    let mut editor = Editor::new();
    type_text(&mut editor, "look");
    editor.paste_image("/p/image-1.png", 1);
    assert_eq!(
        editor.take(),
        "look @\"/p/image-1.png\" ",
        "the chip's word and the space after it"
    );
}

/// An image chip never opens: Tab and a click leave it folded.
#[test]
fn image_paste_chip_never_opens() {
    let mut editor = Editor::new();
    editor.paste_image("/p/image-1.png", 1);
    editor.left();
    assert_eq!(
        editor.selected_paste(),
        Some(0),
        "the edit point is on the chip"
    );
    assert!(!editor.toggle_paste(), "an image chip answers no");
    assert!(!editor.pastes()[0].is_open(), "and stays folded");
    assert!(!editor.fold_paste(0), "a folded image chip folds no more");
    assert_eq!(text(&editor), format!("{MARK} "), "the text is the same");
}

/// A backspace on the mark takes the chip whole, and one undo brings it back
/// with its number.
#[test]
fn image_paste_chip_goes_whole_on_backspace_and_one_undo_brings_it_back() {
    let mut editor = Editor::new();
    type_text(&mut editor, "look");
    editor.paste_image("/p/image-1.png", 1);
    editor.left();
    editor.backspace();
    assert_eq!(
        marked(&editor),
        "look| ",
        "the chip went whole and the space after it stayed"
    );
    assert!(editor.pastes().is_empty(), "and left nothing behind");
    editor.undo();
    assert_eq!(editor.pastes().len(), 1, "one undo puts it back");
    assert_eq!(editor.pastes()[0].image(), Some(1), "with its number");
    assert_eq!(editor.take(), "look @\"/p/image-1.png\" ", "and its word");
}

/// A kill takes the image chip into the ring whole, and the yank puts it back
/// with the number its label draws.
#[test]
fn image_paste_chip_keeps_its_number_through_kill_and_yank() {
    let mut editor = Editor::new();
    type_text(&mut editor, "look");
    editor.paste_image("/p/image-3.png", 3);
    editor.insert(" now");
    editor.line_start();
    editor.kill_line();
    assert!(editor.pastes().is_empty(), "the kill takes the chip whole");
    editor.yank();
    assert_eq!(editor.pastes().len(), 1, "the yank puts it back");
    assert_eq!(editor.pastes()[0].image(), Some(3), "with its number");
    assert_eq!(
        editor.take(),
        "look @\"/p/image-3.png\"  now",
        "and its word in place"
    );
}

#[test]
fn image_paste_one_undo_takes_the_chip_and_its_space() {
    let mut editor = Editor::new();
    editor.insert("look ");
    editor.paste_image("/p/image-1.png", 1);
    editor.undo();
    assert_eq!(text(&editor), "look ");
    assert!(editor.pastes().is_empty());
}
