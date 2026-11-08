//! Rendering tests: one per construct, plus the streaming rules.

use crate::theme::{Depth, Mode, Overrides, Rgb, Role, Theme};

use super::layout::width;
use super::*;

/// A reply of the shape a model writes: headings, prose, a list, a table and a
/// fenced code block.
const REPLY: &str = "\
# Fix the test run

The suite fails on `cargo test`. Run **nextest** instead, and keep
`--retries 0`. See [the guide](https://example.com/guide) for the reason.

## Steps

1. Read the log.
2. Retry the two failing tests:
   - `session::resume`
   - `session::fork`
3. Report.

- [x] read the log
- [ ] retry

| test | status |
|------|--------|
| resume | pass |
| fork | fail |

```rust
let report = run(&suite);
assert!(report.ok());
```

> Retries hide a flake; fix the test.

---

Done.
";

/// Every fixture the streaming test feeds both ways.
const FIXTURES: [(&str, &str); 15] = [
    ("reply", REPLY),
    ("headings", "# One\ntwo\n===\n\n### Three\n"),
    ("list", "- one\n- two\n  - nested\n\n1. first\n2. second\n"),
    ("table", "| a | b |\n|---|---|\n| 1 | 22 |\n"),
    ("code", "```rust\nlet x = 1;\n\nassert!(x);\n```\n"),
    ("quote", "> one\n> > two\n\nafter\n"),
    (
        "inline",
        "plain **bold** then *italic* and ~~gone~~ with `code`.\n",
    ),
    (
        "link",
        "See [the guide](https://example.com/guide) and [b](/b).\n",
    ),
    ("breaks", "one  \ntwo\nthree\n\n---\n\nlast\n"),
    ("task", "- [x] done\n- [ ] todo\n"),
    ("pentable", "| a | b |\n|---|"),
    ("openfence", "text\n\n```\nlet x = 1;\n"),
    (
        "nested",
        "- item one\n\n  ```sh\n  cargo test\n  ```\n\n- item two\n",
    ),
    ("quotedlist", "> - one\n> - two\n>\n> after\n"),
    ("entities", "a &amp; b <b>c</b> `d &amp; e`\n"),
];

/// A message rendered and finished.
fn render(text: &str, cols: u16) -> Markdown {
    let mut markdown = Markdown::new(cols);
    markdown.push(text);
    markdown.finish();
    markdown
}

/// Every drawn line, committed first, as plain text.
fn rows(markdown: &Markdown) -> Vec<String> {
    let (committed, live) = markdown.lines();
    committed
        .iter()
        .chain(live)
        .map(|line| markdown.line_text(line))
        .collect()
}

/// The style of the first piece whose text is `needle`.
fn style_of(markdown: &Markdown, needle: &str) -> Style {
    let (committed, live) = markdown.lines();
    for piece in committed.iter().chain(live).flat_map(Line::pieces) {
        if markdown.piece_text(piece) == needle {
            return markdown.styles().get(piece.style());
        }
    }
    panic!("no piece says {needle:?} in {:?}", rows(markdown));
}

/// A renderer for one message with the roles of a rich dark theme, so a role's
/// color is not the one the built-in look carries.
fn themed(text: &str, cols: u16) -> (Markdown, Theme) {
    let (mut markdown, theme) = themed_new(cols);
    markdown.push(text);
    markdown.finish();
    (markdown, theme)
}

/// A renderer with the roles of a rich dark theme and no text yet.
fn themed_new(cols: u16) -> (Markdown, Theme) {
    let theme = Theme::resolve(
        Mode::Rich {
            background: Rgb::new(0x16, 0x16, 0x16),
            dark: true,
        },
        Depth::TrueColor,
        &Overrides::default(),
    );
    let mut markdown = Markdown::new(cols);
    markdown.set_theme(&theme);
    (markdown, theme)
}

/// Every drawn piece of a message, as its text and its style.
fn drawn(markdown: &Markdown) -> Vec<(String, Style)> {
    let (committed, live) = markdown.lines();
    committed
        .iter()
        .chain(live)
        .flat_map(Line::pieces)
        .map(|piece| {
            (
                markdown.piece_text(piece).to_string(),
                markdown.styles().get(piece.style()),
            )
        })
        .collect()
}

#[test]
fn a_paragraph_and_a_list_have_one_blank_row_between() {
    let markdown = render("intro\n\n- one\n- two\n", 40);
    assert_eq!(rows(&markdown), ["intro", "", "• one", "• two"]);
}

#[test]
fn a_heading_drops_its_marks() {
    let (markdown, theme) = themed("## Title\n\nbody\n", 40);
    assert_eq!(rows(&markdown), ["Title", "", "body"]);
    let heading = style_of(&markdown, "Title");
    assert!(heading.bold, "the heading is not bold");
    assert_eq!(
        heading.fg,
        theme.style(Role::Heading).fg,
        "the heading is not in the heading role"
    );
    assert_ne!(
        heading.fg,
        style_of(&markdown, "body").fg,
        "the heading is the body's color"
    );
}

#[test]
fn a_wrapped_list_item_hangs_under_its_text() {
    let markdown = render("- abcdefghi\n", 10);
    assert_eq!(rows(&markdown), ["• abcdefgh", "  i"]);
    assert!(style_of(&markdown, "• ").dim, "the bullet is not dim");
}

#[test]
fn a_numbered_item_hangs_under_its_text() {
    let markdown = render("10. abcdefghij\n", 12);
    assert_eq!(rows(&markdown), ["10. abcdefgh", "    ij"]);
    assert!(style_of(&markdown, "10. ").dim, "the number is not dim");
}

#[test]
fn a_quote_has_the_bar() {
    let (markdown, theme) = themed("> one\n> > two\n", 40);
    assert_eq!(rows(&markdown), ["▎ one", "▎ ▎ two"]);
    assert_eq!(
        style_of(&markdown, "▎ ").fg,
        theme.style(Role::QuoteBar).fg,
        "the bar is not in the quote bar role"
    );
    let text = style_of(&markdown, "one");
    assert!(text.italic, "the quote text is not italic");
    assert!(text.dim, "the quote text is not dim");
}

#[test]
fn inline_code_has_one_space_of_fill_each_side() {
    let markdown = render("run `ls` now\n", 40);
    assert_eq!(rows(&markdown), ["run  ls  now"]);
    let pieces = markdown.lines().0[0].pieces();
    let at = pieces
        .iter()
        .position(|piece| markdown.piece_text(piece) == "ls")
        .expect("the code span");
    let code = markdown.styles().get(pieces[at].style());
    assert_ne!(code.bg, Color::Default, "the code has no fill: {code:?}");
    for side in [at - 1, at + 1] {
        assert_eq!(markdown.piece_text(&pieces[side]), " ");
        assert_eq!(
            markdown.styles().get(pieces[side].style()),
            code,
            "the fill does not cover the pad"
        );
    }
}

/// A theme with no fill interns the code style as the body's, so the code pad
/// must come from the span itself: bold keeps one space on each side and inline
/// code keeps its two.
#[test]
fn bold_spacing_in_a_theme_with_no_fill() {
    for mode in [Mode::Terminal, Mode::NoColor] {
        let theme = Theme::resolve(mode, Depth::Ansi256, &Overrides::default());
        let mut markdown = Markdown::new(40);
        markdown.set_theme(&theme);
        markdown.push("Some **bold** text with `code` end\n");
        markdown.finish();
        assert_eq!(
            rows(&markdown),
            ["Some bold text with  code  end"],
            "{mode:?} pads a piece that is not code"
        );
    }
}

#[test]
fn streaming_a_second_paragraph_adds_its_blank_row_once() {
    let mut markdown = Markdown::new(40);
    markdown.push("one\n\n");
    assert_eq!(rows(&markdown), ["one"], "a blank row came before a block");
    markdown.push("two");
    assert_eq!(rows(&markdown), ["one", "", "two"]);
    markdown.push(" more\n\n");
    assert_eq!(rows(&markdown), ["one", "", "two more"]);
    markdown.push("three");
    assert_eq!(
        rows(&markdown),
        ["one", "", "two more", "", "three"],
        "two blank rows in a row"
    );
    markdown.finish();
    assert_eq!(rows(&markdown), ["one", "", "two more", "", "three"]);
}

#[test]
fn markdown_headings_are_bold_in_the_heading_role() {
    let markdown = render(
        "# One\n\n## Two\n\n### Three\n\nTitle\n===\n\nSub\n---\n",
        40,
    );
    let rows = rows(&markdown);
    assert_eq!(
        rows,
        ["One", "", "Two", "", "Three", "", "Title", "", "Sub"]
    );
    let mut seen = Vec::new();
    for word in ["One", "Two", "Three", "Title", "Sub"] {
        let style = style_of(&markdown, word);
        assert!(style.bold, "{word} is not bold");
        seen.push(style);
    }
    assert_eq!(seen[0], seen[3], "a setext h1 is not the atx h1 style");
    assert_eq!(seen[1], seen[4], "a setext h2 is not the atx h2 style");
    assert!(
        seen.windows(2).all(|pair| pair[0] == pair[1]),
        "each heading level has its own style"
    );
}

#[test]
fn markdown_lists_indent_per_level_and_number_per_item() {
    let markdown = render("- one\n  - nested\n- two\n\n3. third\n4. fourth\n", 40);
    assert_eq!(
        rows(&markdown),
        ["• one", "  • nested", "• two", "", "3. third", "4. fourth"]
    );
}

#[test]
fn markdown_task_list_marks_each_item() {
    let markdown = render("- [x] done\n- [ ] todo\n", 40);
    assert_eq!(rows(&markdown), ["[x] done", "[ ] todo"]);
}

#[test]
fn markdown_code_blocks_label_and_wrap() {
    let markdown = render("```rust\nlet x = 1;\nlet y = 22;\n```\n", 20);
    assert_eq!(
        rows(&markdown),
        [
            "               rust ",
            "  let x = 1;        ",
            "  let y = 22;       ",
            "                    "
        ]
    );

    let indented = render("    indented\n    code\n", 20);
    assert_eq!(
        rows(&indented),
        [
            "                    ",
            "  indented          ",
            "  code              ",
            "                    "
        ]
    );

    let long = render("```\nabcdefghij\n```\n", 5);
    assert_eq!(
        rows(&long),
        ["     ", "  abc", "  def", "  ghi", "  j  ", "     "]
    );
}

/// The code block of the mockup: the fill across the width one row above the
/// code and one below it, and the fence's tag dim one column from the right end
/// of the row above.
#[test]
fn code_block_has_padding_rows_and_its_label() {
    let (markdown, theme) = themed("```rust\nlet x = 1;\n```\n", 20);
    assert_eq!(
        rows(&markdown),
        [
            "               rust ",
            "  let x = 1;        ",
            "                    "
        ],
        "the block has no padding rows or its label is not at the right"
    );
    let fill = theme.fill(Role::CodeFill);
    assert_ne!(fill, Color::Default, "the theme has no code fill");
    for row in rows(&markdown) {
        assert_eq!(width(&row), 20, "{row:?} does not reach the width");
    }
    let (committed, live) = markdown.lines();
    for line in committed.iter().chain(live) {
        let pieces = line.pieces();
        for piece in [pieces[0], pieces[pieces.len() - 1]] {
            let style = markdown.styles().get(piece.style());
            assert_eq!(style.bg, fill, "a row does not hold the block's fill");
        }
    }
    let label = style_of(&markdown, "rust");
    assert!(label.dim, "the label is not dim");
    assert_eq!(label.bg, fill, "the label does not sit on the fill");

    // A fence without a tag has no label.
    let bare = render("```\nlet x = 1;\n```\n", 20);
    assert_eq!(
        rows(&bare)[0],
        " ".repeat(20),
        "a tagless fence has a label"
    );
}

#[test]
fn code_block_highlights_rust_keywords() {
    let (markdown, theme) = themed("```rust\nlet x = 1;\n```\n", 40);
    let keyword = theme.style(Role::CodeKeyword).fg;
    let number = theme.style(Role::CodeNumber).fg;
    assert_ne!(keyword, theme.style(Role::Text).fg, "a keyword is plain");
    let pieces = drawn(&markdown);
    assert!(
        pieces
            .iter()
            .any(|(text, style)| text == "let" && style.fg == keyword),
        "`let` is not a keyword: {pieces:?}"
    );
    assert!(
        pieces
            .iter()
            .any(|(text, style)| text == "1" && style.fg == number),
        "`1` is not a number: {pieces:?}"
    );
    assert!(
        !pieces
            .iter()
            .any(|(text, style)| text.contains('x') && style.fg == keyword),
        "`x` is a keyword: {pieces:?}"
    );
    let fill = theme.fill(Role::CodeFill);
    assert!(
        pieces.iter().all(|(_, style)| style.bg == fill),
        "a token has no fill behind it: {pieces:?}"
    );
}

#[test]
fn code_block_unknown_tag_is_plain() {
    let (markdown, theme) = themed("```mmm\nlet x = 1;\n```\n", 40);
    let keyword = theme.style(Role::CodeKeyword).fg;
    let pieces = drawn(&markdown);
    assert!(
        pieces.iter().any(|(text, _)| text == "let x = 1;"),
        "an unknown tag split the line into tokens: {pieces:?}"
    );
    assert!(
        !pieces.iter().any(|(_, style)| style.fg == keyword),
        "an unknown tag has token colors: {pieces:?}"
    );
    let bare = drawn(&render("```\nlet x = 1;\n```\n", 40));
    assert!(
        bare.iter().any(|(text, _)| text == "let x = 1;"),
        "a fence without a tag split the line into tokens: {bare:?}"
    );
}

/// A line is tokenized once it has ended, and the open line draws plain until
/// its newline arrives.
#[test]
fn code_block_streams_a_line_at_a_time() {
    let (mut markdown, theme) = themed_new(40);
    let keyword = theme.style(Role::CodeKeyword).fg;
    let keywords = |markdown: &Markdown| {
        drawn(markdown)
            .iter()
            .filter(|(_, style)| style.fg == keyword)
            .count()
    };
    markdown.push("```rust\n");
    markdown.push("let x = 1;\n");
    assert_eq!(keywords(&markdown), 1, "the ended line is not highlighted");
    markdown.push("let y = ");
    assert_eq!(keywords(&markdown), 1, "the open line is highlighted");
    let pieces = drawn(&markdown);
    assert!(
        pieces.iter().any(|(text, _)| text == "let y = "),
        "the open line is not plain: {pieces:?}"
    );
    markdown.push("2;\n");
    assert_eq!(
        keywords(&markdown),
        2,
        "the line is not highlighted once it ended"
    );
    markdown.finish();
    assert_eq!(
        rows(&markdown),
        rows(&render(markdown.text(), 40)),
        "the streamed block is not what its text says"
    );
}

/// The state a line leaves open reaches the next one: a block comment that
/// started on an earlier line is a comment on the line that closes it.
#[test]
fn code_block_carries_a_comment_across_lines() {
    let (markdown, theme) = themed(
        "```rust\n/* a note\n   that goes on */\nlet x = 1;\n```\n",
        40,
    );
    let comment = theme.style(Role::CodeComment);
    let pieces = drawn(&markdown);
    for text in ["/* a note", "   that goes on */"] {
        let style = pieces
            .iter()
            .find(|(piece, _)| piece == text)
            .unwrap_or_else(|| panic!("no piece {text:?} in {pieces:?}"))
            .1;
        assert_eq!(style.fg, comment.fg, "{text:?} is not a comment");
        assert!(style.italic, "{text:?} is not italic");
    }
    assert!(
        pieces
            .iter()
            .any(|(text, style)| text == "let" && style.fg == theme.style(Role::CodeKeyword).fg),
        "the code after the comment is not code: {pieces:?}"
    );
}

/// Terminal mode has no fill, so the block keeps its rows and its label but
/// draws nothing across the width.
#[test]
fn code_block_terminal_mode_has_no_fill() {
    let theme = Theme::resolve(Mode::Terminal, Depth::Ansi256, &Overrides::default());
    let mut markdown = Markdown::new(20);
    markdown.set_theme(&theme);
    markdown.push("```rust\nlet x = 1;\n```\n");
    markdown.finish();
    assert_eq!(rows(&markdown), ["               rust", "  let x = 1;", ""]);
    assert_eq!(
        theme.fill(Role::CodeFill),
        Color::Default,
        "the terminal mode has a code fill"
    );
}

#[test]
fn markdown_quote_has_a_bar_per_level() {
    let markdown = render("> one\n> > two\n", 40);
    assert_eq!(rows(&markdown), ["▎ one", "▎ ▎ two"]);
}

#[test]
fn markdown_table_columns_fit_their_content() {
    let markdown = render("| a | bb |\n|---|---|\n| ccc | d |\n", 40);
    assert_eq!(rows(&markdown), ["a    bb", "───────", "ccc  d"]);
    assert!(style_of(&markdown, "a").bold, "a table header is not bold");
}

#[test]
fn markdown_wide_table_becomes_key_value_lines() {
    let table = "| test | status | notes |\n|------|--------|-------|\n\
                 | resume | pass | reads the journal |\n| fork | fail | rewrites the head |\n";
    let markdown = render(table, 30);
    let rows = rows(&markdown);
    assert!(
        rows.iter().all(|row| width(row) <= 30),
        "a row is wider than the width: {rows:?}"
    );
    assert!(rows.contains(&"test: resume".to_string()), "{rows:?}");
    assert!(
        rows.iter().any(|row| row.starts_with("status: pass")),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|row| row.contains('|')), "{rows:?}");
}

#[test]
fn markdown_link_label_styles_and_shows_its_url() {
    let markdown = render("See [the guide](https://example.com) now.\n", 60);
    assert_eq!(
        rows(&markdown),
        ["See the guide (https://example.com) now."]
    );
    assert!(
        style_of(&markdown, "guide").underline,
        "the label is not a link"
    );

    let mut osc8 = render("See [the guide](https://example.com) now.\n", 60);
    osc8.set_osc8(true);
    assert_eq!(rows(&osc8), ["See the guide now."]);
}

#[test]
fn markdown_inline_styles() {
    let markdown = render("a **bold** *italic* ~~gone~~ `code` end\n", 60);
    assert_eq!(rows(&markdown), ["a bold italic gone  code  end"]);
    assert!(style_of(&markdown, "bold").bold);
    assert!(style_of(&markdown, "italic").italic);
    assert!(style_of(&markdown, "gone").strikethrough);
    assert_ne!(style_of(&markdown, "code").bg, Color::Default);
}

#[test]
fn markdown_rule_and_breaks() {
    let markdown = render("one  \ntwo\nthree\n\n---\n", 10);
    assert_eq!(rows(&markdown), ["one", "two three", "", "──────────"]);
}

#[test]
fn markdown_html_and_math_are_plain() {
    let markdown = render("<span>raw</span> and $x^2$ here\n", 60);
    assert_eq!(rows(&markdown), ["<span>raw</span> and $x^2$ here"]);
}

#[test]
fn markdown_wraps_at_the_width() {
    let markdown = render("alpha beta gamma delta epsilon\n", 12);
    assert_eq!(rows(&markdown), ["alpha beta", "gamma delta", "epsilon"]);
    let long = render("abcdefghijklmnop\n", 6);
    assert_eq!(rows(&long), ["abcdef", "ghijkl", "mnop"]);
    let wide = render("世 世 世\n", 5);
    assert_eq!(rows(&wide), ["世 世", "世"]);
}

#[test]
fn markdown_unclosed_markers_close_while_the_block_is_open() {
    let mut markdown = Markdown::new(60);
    markdown.push("plain **bold");
    assert_eq!(rows(&markdown), ["plain bold"]);
    assert!(style_of(&markdown, "bold").bold);
    markdown.push(" and `code");
    assert_eq!(rows(&markdown), ["plain bold and  code"]);
    markdown.push("` end**");
    assert_eq!(rows(&markdown), ["plain bold and  code  end"]);
    assert_ne!(style_of(&markdown, "code").bg, Color::Default);
}

/// The line the review streamed: at every prefix, a marker whose closer has not
/// arrived stays hidden, as the finished line hides it.
#[test]
fn markdown_unclosed_markers_never_show_while_streaming() {
    const LINE: &str = "Some **bold** and *it* and `code` and ~~gone~~ [l](http://x.y)";
    let mut markdown = Markdown::new(80);
    let mut buf = [0; 4];
    for c in LINE.chars() {
        markdown.push(c.encode_utf8(&mut buf));
        let drawn = rows(&markdown).join("\n");
        for marker in ['*', '_', '`', '~', '[', ']'] {
            assert!(
                !drawn.contains(marker),
                "{marker:?} showed after {:?}: {drawn:?}",
                markdown.text()
            );
        }
    }
    markdown.finish();
    assert_eq!(
        rows(&markdown),
        ["Some bold and it and  code  and gone l (http://x.y)"]
    );
}

#[test]
fn markdown_marker_prefix_is_counted_once() {
    // The bullet takes 2 of the 10 columns, so the 8-column word still fits.
    assert_eq!(rows(&render("- abcdefgh\n", 10)), ["• abcdefgh"]);
    assert_eq!(rows(&render("- abcdefghi\n", 10)), ["• abcdefgh", "  i"]);
    // A quote bar takes 2 columns as well.
    assert_eq!(rows(&render("> abcdefgh\n", 10)), ["▎ abcdefgh"]);
    assert_eq!(rows(&render("> abcdefghi\n", 10)), ["▎ abcdefgh", "▎ i"]);
}

#[test]
fn markdown_wraps_a_vs16_emoji_as_one_grapheme() {
    // `❤\u{FE0F}` is two columns as a whole and one and zero by character, so a
    // per-character fit lets a row be a column too wide.
    let markdown = render("a❤\u{FE0F}x\n", 3);
    let rows = rows(&markdown);
    assert_eq!(rows, ["a❤\u{FE0F}", "x"]);
    assert!(rows.iter().all(|row| width(row) <= 3), "{rows:?}");
}

/// A bracket after a word is an index, not the start of a link, so it shows as
/// written and no closer is added for it.
#[test]
fn markdown_index_brackets_show_as_written() {
    let mut markdown = Markdown::new(40);
    markdown.push("arr[0]");
    assert_eq!(rows(&markdown), ["arr[0]"]);
    markdown.push(" and arr[");
    // A marker at the very end waits for the next delta, as before.
    assert_eq!(rows(&markdown), ["arr[0] and arr"]);
    markdown.push("1]");
    assert_eq!(rows(&markdown), ["arr[0] and arr[1]"]);
}

/// A destination that never closes gets its closer, and the text past the end of
/// the message never shows: an invalid destination stays as written.
#[test]
fn markdown_unclosed_destination_keeps_its_own_text() {
    let mut markdown = Markdown::new(80);
    markdown.push("see [l](x y");
    assert_eq!(rows(&markdown), ["see [l](x y"]);
    markdown.push(" and more");
    assert_eq!(rows(&markdown), ["see [l](x y and more"]);
}

/// A reference link whose definition streams in a later block is kept as
/// written: the block that holds it is finished before the definition arrives.
#[test]
fn markdown_reference_link_defined_later_stays_as_written() {
    let mut markdown = Markdown::new(80);
    markdown.push("See [docs][1].\n\n");
    markdown.push("next\n\n");
    markdown.push("[1]: http://x.y\n");
    markdown.finish();
    assert_eq!(rows(&markdown), ["See [docs][1].", "", "next"]);
}

/// A plain delta re-lays only the last line of the open block, so its work does
/// not follow the block: a 4 KB paragraph, a 40 KB paragraph and a 40 KB fence
/// lay out one line each, and so does a delta into the fence that is not plain.
#[test]
fn markdown_plain_delta_lays_out_only_the_last_line() {
    let mut paragraph = Markdown::new(40);
    paragraph.push(&"word ".repeat(8_000));
    let mut fence = Markdown::new(40);
    fence.push("```\n");
    fence.push(&"let x = 1;\n".repeat(4_000));
    let mut short = Markdown::new(40);
    short.push("open paragraph");
    for markdown in [&mut short, &mut paragraph, &mut fence] {
        markdown.push("one ");
        layout::laid_reset();
        for _ in 0..20 {
            markdown.push("word ");
        }
        let laid = layout::laid();
        assert_eq!(laid, 20, "{laid} lines for 20 deltas of one line");
    }
    // Punctuation takes the fence path rather than the plain one, and lays out
    // the last line as well: 4000 lines before it would be 4000 rows per delta.
    fence.push("\n");
    layout::laid_reset();
    for _ in 0..20 {
        fence.push("if x > 1 { ");
    }
    let laid = layout::laid();
    assert_eq!(laid, 20, "{laid} lines for 20 deltas into an open fence");
    // A delta that starts a line lays out the line the mark names and the one it
    // starts, not the four thousand before them.
    layout::laid_reset();
    for _ in 0..20 {
        fence.push("if x > 1 {\n");
    }
    let laid = layout::laid();
    assert!(laid <= 40, "{laid} lines for 20 deltas that start a line");
}

#[test]
fn markdown_trailing_marker_waits_for_its_partner() {
    let mut markdown = Markdown::new(60);
    markdown.push("emph ");
    markdown.push("*");
    assert_eq!(rows(&markdown), ["emph"], "a lone marker showed");
    markdown.push("word*");
    assert_eq!(rows(&markdown), ["emph word"]);
    assert!(style_of(&markdown, "word").italic);
}

#[test]
fn markdown_open_fence_streams_as_code() {
    let mut markdown = Markdown::new(20);
    markdown.push("text\n\n```rust\nlet x = 1;\n");
    let block = [
        "text",
        "",
        "               rust ",
        "  let x = 1;        ",
        "                    ",
    ];
    assert_eq!(rows(&markdown), block);
    markdown.push("``");
    assert_eq!(rows(&markdown), block, "a partial closing fence showed");
    markdown.push("`");
    markdown.finish();
    assert_eq!(rows(&markdown), block);
}

#[test]
fn markdown_table_waits_for_its_delimiter_row() {
    let mut markdown = Markdown::new(30);
    markdown.push("intro\n\n| a | b |");
    assert_eq!(
        rows(&markdown),
        ["intro"],
        "a table header shows before its delimiter row"
    );
    markdown.push("\n|---|---");
    let grid = rows(&markdown);
    assert!(grid.contains(&"a  b".to_string()), "{grid:?}");
    markdown.push("\n| 1 | 2 |");
    let filled = rows(&markdown);
    assert!(filled.contains(&"1  2".to_string()), "{filled:?}");
}

#[test]
fn markdown_growing_list_holds_its_last_line() {
    let mut markdown = Markdown::new(40);
    markdown.push("- one\n- two");
    assert_eq!(
        rows(&markdown),
        ["• one"],
        "a list item's last line showed before its line ended"
    );
    markdown.push(" more\n");
    assert_eq!(rows(&markdown), ["• one", "• two more"]);
}

#[test]
fn markdown_link_destination_is_kept_by_label_span() {
    let markdown = render("See [the guide](https://example.com) now.\n", 60);
    let (committed, _) = markdown.lines();
    let label = committed[0]
        .pieces()
        .iter()
        .find(|piece| markdown.piece_text(piece) == "guide")
        .copied()
        .expect("the label");
    assert_eq!(markdown.link_of(&label), Some("https://example.com"));
    let plain = committed[0].pieces()[0];
    assert_eq!(markdown.link_of(&plain), None);
}

#[test]
fn markdown_empty_message_has_no_lines() {
    let markdown = render("", 40);
    assert_eq!(rows(&markdown), Vec::<String>::new());
    let blank = render("\n\n", 40);
    assert_eq!(rows(&blank), Vec::<String>::new());
}

#[test]
fn markdown_list_growth_keeps_earlier_items() {
    let mut markdown = Markdown::new(40);
    markdown.push("- one\n- two\n");
    markdown.push("- three\n\nplain\n");
    let kept = rows(&markdown);
    assert_eq!(kept, ["• one", "• two", "• three", "", "plain"]);
    markdown.push("text\n");
    assert_eq!(
        rows(&markdown),
        ["• one", "• two", "• three", "", "plain text"]
    );
}

#[test]
fn markdown_stream_matches_batch_for_every_fixture() {
    for (name, fixture) in FIXTURES {
        for cols in [20, 40, 80] {
            let batch = rows(&render(fixture, cols));
            let mut markdown = Markdown::new(cols);
            let mut buf = [0; 4];
            for c in fixture.chars() {
                markdown.push(c.encode_utf8(&mut buf));
            }
            markdown.finish();
            assert_eq!(rows(&markdown), batch, "{name} at {cols} columns by char");
            let mut markdown = Markdown::new(cols);
            for chunk in fixture.as_bytes().chunks(3) {
                if let Ok(text) = std::str::from_utf8(chunk) {
                    markdown.push(text);
                }
            }
            markdown.finish();
            assert_eq!(rows(&markdown), batch, "{name} at {cols} columns by 3");
        }
    }
}

#[test]
fn markdown_finished_block_is_parsed_once() {
    let mut markdown = Markdown::new(40);
    markdown.push("# Title\n\nfirst paragraph.\n\nsecond ");
    let frozen = markdown.frozen_end();
    assert!(frozen > 0, "the heading is not frozen");
    let before = markdown.parses();
    for _ in 0..20 {
        markdown.push("word ");
    }
    markdown.push("end.\n\nthird section\n");
    let after = markdown.parses();
    assert!(after > before, "nothing was parsed after the first block");
    for range in markdown.parse_ranges()[before..].iter() {
        assert!(
            range.start >= frozen,
            "{range:?} re-parsed the frozen block that ends at {frozen}"
        );
    }
}

#[test]
fn markdown_width_change_lays_out_without_a_parse() {
    let mut markdown = render(REPLY, 60);
    let wide = rows(&markdown);
    let before = markdown.parses();
    markdown.set_width(32);
    assert_eq!(markdown.parses(), before, "a width change parsed again");
    let narrow = rows(&markdown);
    assert_ne!(wide, narrow);
    assert!(
        narrow.iter().all(|row| width(row) <= 32),
        "{narrow:?} does not fit 32 columns"
    );
    markdown.set_width(60);
    assert_eq!(rows(&markdown), wide, "the wide layout is not reproducible");
}

#[test]
fn markdown_pieces_are_ranges_into_the_message() {
    let markdown = render(REPLY, 80);
    let text = markdown.text();
    let (committed, live) = markdown.lines();
    let mut pieces = 0;
    for piece in committed.iter().chain(live).flat_map(Line::pieces) {
        if piece.source() != Source::Text {
            continue;
        }
        pieces += 1;
        let range = piece.range();
        assert_eq!(&text[range], markdown.piece_text(piece), "a copied span");
    }
    assert!(pieces > 50, "only {pieces} spans of the message text");
    for piece in committed.iter().chain(live).flat_map(Line::pieces) {
        if piece.source() == Source::Lit {
            assert_eq!(markdown.piece_text(piece), " ", "copied text: {piece:?}");
        }
    }
}

#[test]
fn markdown_committed_and_live_lines_are_separate() {
    let mut markdown = Markdown::new(40);
    markdown.push("first block\n\nopen block");
    let (committed, live) = markdown.lines();
    assert_eq!(
        committed
            .iter()
            .map(|line| markdown.line_text(line))
            .collect::<Vec<_>>(),
        ["first block"]
    );
    assert_eq!(
        live.iter()
            .map(|line| markdown.line_text(line))
            .collect::<Vec<_>>(),
        ["", "open block"]
    );
    markdown.finish();
    let (committed, live) = markdown.lines();
    assert!(live.is_empty(), "a finished reply keeps live lines");
    assert_eq!(committed.len(), 3);
}

#[test]
fn markdown_model_reply_renders_every_construct() {
    let markdown = render(REPLY, 60);
    let rows = rows(&markdown);
    for expected in [
        "Fix the test run",
        "Steps",
        "1. Read the log.",
        "  • session::resume",
        "[x] read the log",
        "[ ] retry",
        "test    status",
        "resume  pass",
        "▎ Retries hide a flake; fix the test.",
        "Done.",
    ] {
        assert!(
            rows.iter().any(|row| row == expected),
            "{expected:?} is missing from {rows:#?}"
        );
    }
    assert!(
        rows.iter()
            .all(|row| !row.contains("**") && !row.contains("```")),
        "markers survived: {rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("────")),
        "no rule: {rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.trim() == "rust"),
        "the fence's tag is not on a row of its own: {rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.trim() == "let report = run(&suite);"),
        "the code block's line is missing: {rows:#?}"
    );
}

#[test]
fn markdown_nested_constructs() {
    assert_eq!(
        rows(&render(
            "- item one\n\n  ```sh\n  cargo test\n  ```\n\n- item two\n",
            40
        )),
        ["• item one", "  sh", "  cargo test", "• item two"]
    );
    assert_eq!(
        rows(&render("> - one\n> - two\n>\n> after\n", 40)),
        ["▎ • one", "▎ • two", "▎ after"]
    );
    assert_eq!(
        rows(&render(
            "1. one\n\n   second paragraph of one\n\n2. two\n",
            40
        )),
        ["1. one", "   second paragraph of one", "2. two"]
    );
    assert_eq!(
        rows(&render("a &amp; b <b>c</b>\n", 40)),
        ["a & b <b>c</b>"],
        "an entity is not decoded and raw HTML shows as text"
    );
}

/// Rows of the same text as one push, which is the layout a stream must match
/// at every prefix.
fn batch(text: &str, cols: u16) -> Vec<String> {
    let mut markdown = Markdown::new(cols);
    markdown.push(text);
    rows(&markdown)
}

/// A delta that starts a line after a code line that wrapped keeps the rows the
/// wrapped line already drew: the delta finishes that line from the row the
/// mark names instead of drawing its first row again.
#[test]
fn markdown_fence_break_after_a_wrapped_line_keeps_its_rows() {
    let mut markdown = Markdown::new(20);
    markdown.push("```rust\n");
    markdown.push("let report = run(&suite);");
    let wrapped = rows(&markdown);
    assert_eq!(
        wrapped,
        [
            "               rust ",
            "  let report = run(&",
            "  suite);           ",
            "                    "
        ],
        "the line does not wrap at 20 columns"
    );
    markdown.push("\n");
    assert_eq!(
        rows(&markdown),
        wrapped,
        "the delta repeated the rows of the wrapped line"
    );
    markdown.push("next();\n");
    assert_eq!(
        rows(&markdown),
        [
            "               rust ",
            "  let report = run(&",
            "  suite);           ",
            "  next();           ",
            "                    "
        ]
    );
}

/// At every prefix of a fence whose lines wrap, a streamed block draws the rows
/// a single push of the same text draws.
#[test]
fn markdown_fence_stream_matches_batch_at_every_prefix() {
    const CODE: &str = "```rust\nlet report = run(&suite);\nassert!(report.ok());\n\
        let total = report.count() + report.skipped();\n```\n";
    for cols in [8, 12, 20, 24] {
        let whole = batch(CODE, cols);
        let mut markdown = Markdown::new(cols);
        let mut buf = [0; 4];
        for (at, c) in CODE.char_indices() {
            markdown.push(c.encode_utf8(&mut buf));
            let text = &CODE[..at + c.len_utf8()];
            assert_eq!(
                rows(&markdown),
                batch(text, cols),
                "{:?} streamed at {cols} columns",
                markdown.text()
            );
        }
        markdown.finish();
        assert_eq!(rows(&markdown), whole, "the fence at {cols} columns");
        assert_eq!(whole, rows(&render(CODE, cols)));
    }
}

/// A backtick inside a line of code does not cost the whole fence: only a line
/// of three or more fence characters can close the block, so a delta that holds
/// a single backtick is appended without a parse.
#[test]
fn markdown_backtick_in_a_code_line_does_not_parse_again() {
    const DELTA: &str = "echo `date` and `hostname` now ";
    let mut markdown = Markdown::new(200);
    markdown.push("```sh\n");
    // Forty kilobytes of code, so a parse per delta would follow the block.
    markdown.push(&"echo hi\n".repeat(5_000));
    let before = markdown.parses();
    for _ in 0..1_000 {
        markdown.push(DELTA);
    }
    assert_eq!(
        markdown.parses(),
        before,
        "{} parses for 1000 deltas with a backtick",
        markdown.parses() - before
    );
    markdown.push("\n```\n");
    markdown.finish();
    assert_eq!(
        rows(&markdown),
        rows(&render(markdown.text(), 200)),
        "the streamed fence is not what its text says"
    );
}

/// A closing fence still arrives as one: the line of fence characters takes the
/// parse path even though the deltas before it did not.
#[test]
fn markdown_closing_fence_after_backticks_still_closes() {
    let mut markdown = Markdown::new(20);
    markdown.push("```sh\n");
    markdown.push("echo `date`\n");
    markdown.push("```\n");
    markdown.finish();
    assert_eq!(
        rows(&markdown),
        [
            "                 sh ",
            "  echo `date`       ",
            "                    "
        ],
        "the closing fence showed as code"
    );
    assert!(markdown.frozen_end() > 0, "the fence is not frozen");
    assert_eq!(
        rows(&markdown),
        rows(&render("```sh\necho `date`\n```\n", 20))
    );
}

/// A CRLF line end is not part of the code line: the `\r` never shows while the
/// block is open.
#[test]
fn markdown_crlf_fence_line_has_no_carriage_return() {
    const CODE: &str = "```rs\r\nlet x = 1;\r\nlet y = 22;\r\n```\r\n";
    for size in [1, 3] {
        let mut markdown = Markdown::new(20);
        for chunk in CODE.as_bytes().chunks(size) {
            let chunk = std::str::from_utf8(chunk).expect("ascii");
            markdown.push(chunk);
            let drawn = rows(&markdown);
            // The line still open may end with the `\r` of a CRLF that has not
            // finished arriving; once the newline is in, no row may.
            if !markdown.text().ends_with('\r') {
                assert!(
                    drawn.iter().all(|row| !row.contains('\r')),
                    "{:?} keeps a carriage return: {drawn:?}",
                    markdown.text()
                );
            }
        }
        markdown.finish();
        assert_eq!(
            rows(&markdown),
            [
                "                 rs ",
                "  let x = 1;        ",
                "  let y = 22;       ",
                "                    "
            ],
            "the fence streamed by {size} bytes"
        );
        assert_eq!(rows(&markdown), rows(&render(CODE, 20)));
    }
}
