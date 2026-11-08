//! The sessions sheet.
//!
//! The sheet lists the sessions of the folder, newest first, and shows the
//! preview of the selected one beside the rows at a hundred columns or more.
//! The rows are built once, when the sheet opens, and the preview when the
//! selection moves or the background read of a row's messages answers, so a
//! key never waits for a journal.

use std::sync::Arc;

use crate::history::{Recent, RecentMessages};
use crate::markdown::Markdown;
use crate::panels::SessionRow;
use crate::screen::{Color, Style};
use crate::theme::{Emphasis, Role, Theme};

use super::super::paint::Wrap;
use super::{Kind, Line, Mark, PreviewRow, Row, Run, Sheet};

/// The word of the title row.
const TITLE: &str = "Sessions";

/// The hint at the right edge of the title row, until a keys file moves one.
const HINT: &str = "enter opens";

/// The keys of the footer row, until a keys file moves one.
const FOOTER: &str = "\u{2191}\u{2193} choose \u{b7} enter opens \u{b7} ctrl+d deletes \u{b7} esc back to your draft";

/// The name of a session that has no title.
const UNTITLED: &str = "untitled";

/// The detail of a session a worker runs now.
const LIVE: &str = "live";

/// The preview line of a session that waits for an approval.
const WAITS: &str = "waits for an approval";

/// The preview lines of a session a worker runs, and of one it does not.
const RUNNING: &str = "live";
const IDLE: &str = "not running";

/// The seconds of an age unit.
const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// The mark before a user turn's words, and the two columns they keep under
/// it, as the transcript draws a turn.
const TURN: &str = "\u{203a} ";
const TURN_AT: u8 = 2;

/// The sessions sheet for the folder's sessions, newest first, `now` in unix
/// seconds for the ages.
///
/// The rows are in the order given, so [`picked`] finds a row by its index. The
/// preview starts on the first session.
pub(crate) fn sessions_sheet(rows: &[SessionRow], now: u64) -> Sheet {
    let mut waits = 0;
    let mut built = Vec::with_capacity(rows.len());
    for row in rows {
        if row.waiting {
            waits += 1;
        }
        built.push(Row {
            kind: Kind::Item,
            mark: if row.waiting {
                Mark::Waits
            } else if row.current {
                Mark::Current
            } else {
                Mark::None
            },
            name: Arc::from(name(row)),
            detail: Arc::from(if row.live { LIVE } else { "" }),
            right: Arc::from(age(now.saturating_sub(row.updated))),
            dim: row.headless,
        });
    }
    let count = if waits == 0 {
        format!("{} in this folder", rows.len())
    } else {
        format!("{} in this folder \u{b7} {waits} waits", rows.len())
    };
    let mut sheet = Sheet::new(TITLE, Arc::from(count), HINT, FOOTER, built).with_previews();
    if let Some(row) = rows.first() {
        sheet.set_preview(preview(row, now));
    }
    sheet
}

/// The preview lines of one session: its own.
pub(crate) fn preview(row: &SessionRow, now: u64) -> Vec<Line> {
    let seconds = now.saturating_sub(row.updated);
    let age = age(seconds);
    let id: String = row.id.chars().take(8).collect();
    let state = if row.live { RUNNING } else { IDLE };
    let meta = if seconds < MINUTE {
        format!("{id} \u{b7} {state} \u{b7} changed {age}")
    } else {
        format!("{id} \u{b7} {state} \u{b7} changed {age} ago")
    };
    let mut lines = Vec::with_capacity(3);
    lines.push(Line {
        text: Arc::from(name(row)),
        dim: false,
    });
    lines.push(Line {
        text: Arc::from(meta),
        dim: true,
    });
    if row.waiting {
        lines.push(Line {
            text: Arc::from(WAITS),
            dim: false,
        });
    }
    lines
}

/// The rows under those lines: every message of `recent` drawn as the
/// transcript draws it, a user turn under its `›` and a reply as its markdown
/// renders, for a pane `width` columns wide.
pub(crate) fn preview_rows(recent: &RecentMessages, width: u16, theme: &Theme) -> Vec<PreviewRow> {
    let fill = theme.fill(Role::InputFill);
    let turn = Arc::from(TURN);
    let mark = filled(theme.styled(Role::Accent, Emphasis::Bold), fill);
    let bold = filled(theme.styled(Role::Text, Emphasis::Bold), fill);
    let mut rows = Vec::new();
    for message in &recent.messages {
        // A blank row sets a message off from the one before it.
        rows.push(PreviewRow::default());
        match message {
            Recent::User(text) => turn_rows(text, width, &turn, mark, bold, &mut rows),
            Recent::Reply(text) => reply_rows(text, width, theme, fill, &mut rows),
        }
    }
    rows
}

/// The rows of a user turn: `›` in the accent at column 0 of the first one and
/// the words bold two columns in, wrapped as [`Wrap`] wraps them.
fn turn_rows(
    source: &str,
    width: u16,
    turn: &Arc<str>,
    mark: Style,
    bold: Style,
    rows: &mut Vec<PreviewRow>,
) {
    let mut first = true;
    for piece in Wrap::new(source, width.saturating_sub(u16::from(TURN_AT))) {
        let mut runs = Vec::with_capacity(2);
        if first {
            runs.push(Run {
                text: Arc::clone(turn),
                style: mark,
            });
        }
        runs.push(Run {
            text: Arc::from(piece),
            style: bold,
        });
        rows.push(PreviewRow {
            start: if first { 0 } else { TURN_AT },
            runs,
        });
        first = false;
    }
}

/// The rows of a reply: its markdown laid out at the pane width, one row per
/// line, each a run per piece in the style the block gives it.
fn reply_rows(text: &str, width: u16, theme: &Theme, fill: Color, rows: &mut Vec<PreviewRow>) {
    let mut md = Markdown::new(width);
    md.set_theme(theme);
    md.push(text);
    md.finish();
    for line in md.lines().0 {
        let mut runs = Vec::with_capacity(line.pieces().len());
        for piece in line.pieces() {
            runs.push(Run {
                text: Arc::from(md.piece_text(piece)),
                style: filled(md.styles().get(piece.style()), fill),
            });
        }
        rows.push(PreviewRow { start: 0, runs });
    }
}

/// `style` over the pane's plain fill: a style with a background of its own,
/// as a code block's, keeps it, so no cell of the pane is left without one.
fn filled(mut style: Style, fill: Color) -> Style {
    if style.bg == Color::Default {
        style.bg = fill;
    }
    style
}

/// The id of the selected session, for opening it.
pub(crate) fn picked(sheet: &Sheet, rows: &[SessionRow]) -> Option<Arc<str>> {
    let (index, _) = sheet.selected()?;
    rows.get(index).map(|row| Arc::clone(&row.id))
}

/// The sheet built again from `rows` after a delete: the filter of `old` stays,
/// and the selection stays on its row, or on the last row when the list got
/// shorter.
pub(crate) fn rebuilt(rows: &[SessionRow], now: u64, old: &Sheet) -> Sheet {
    let mut sheet = sessions_sheet(rows, now);
    sheet.filter_to(&old.filter);
    sheet.select_at(old.selected);
    sheet
}

/// The name of a session: its title, or `untitled` when it has none.
pub(crate) fn name(row: &SessionRow) -> &str {
    if row.title.is_empty() {
        UNTITLED
    } else {
        &row.title
    }
}

/// The age of a session as a row's right edge reads it: `now` under a minute,
/// then `5m`, `3h` and `2d`.
fn age(seconds: u64) -> String {
    if seconds < MINUTE {
        "now".to_string()
    } else if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{}h", seconds / HOUR)
    } else {
        format!("{}d", seconds / DAY)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::panels::SessionRow;

    use super::{DAY, HOUR, MINUTE, picked, preview, preview_rows, sessions_sheet};
    use crate::history::{Recent, RecentMessages};
    use crate::screen::Style;
    use crate::theme::{Depth, Emphasis, Mode, Overrides, Rgb, Role, Theme};

    /// A session of the tests: no worker, no approval, not the current one.
    fn row(id: &str, title: &str, updated: u64) -> SessionRow {
        SessionRow {
            id: Arc::from(id),
            title: Arc::from(title),
            updated,
            live: false,
            waiting: false,
            current: false,
            headless: false,
        }
    }

    /// The text of every preview line.
    fn texts(lines: &[super::Line]) -> Vec<&str> {
        lines.iter().map(|line| &*line.text).collect()
    }

    /// Whether every preview line draws dim.
    fn dims(lines: &[super::Line]) -> Vec<bool> {
        lines.iter().map(|line| line.dim).collect()
    }

    /// The theme of a rich frame: the fills the preview draws on.
    fn rich() -> Theme {
        Theme::resolve(
            Mode::Rich {
                background: Rgb::new(30, 30, 30),
                dark: true,
            },
            Depth::TrueColor,
            &Overrides::default(),
        )
    }

    /// The text of a row of the preview's messages.
    fn row_text(row: &super::PreviewRow) -> String {
        row.runs.iter().map(|run| &*run.text).collect()
    }

    /// The style of the run of a row that holds `word`.
    fn style_of(rows: &[super::PreviewRow], word: &str) -> Style {
        for row in rows {
            if let Some(run) = row.runs.iter().find(|run| run.text.contains(word)) {
                return run.style;
            }
        }
        let drawn: Vec<String> = rows.iter().map(row_text).collect();
        panic!("no run holds {word:?}: {drawn:?}");
    }

    /// The count names the folder and, when one waits, the sessions that do.
    #[test]
    fn the_count_reads_the_rows_and_the_sessions_that_wait() {
        let now = 1_000_000;
        let rows = [row("a", "alpha", now), row("b", "beta", now - HOUR)];
        let sheet = sessions_sheet(&rows, now);
        assert_eq!(sheet.title, "Sessions");
        assert_eq!(&*sheet.count, "2 in this folder");
        assert_eq!(sheet.hint, "enter opens");
        assert_eq!(
            sheet.footer,
            "\u{2191}\u{2193} choose \u{b7} enter opens \u{b7} ctrl+d deletes \u{b7} esc back to your draft"
        );
        assert!(
            sheet.previews,
            "the sheet shows the preview when wide enough"
        );
        assert!(!sheet.columns);
        assert_eq!(sheet.rows.len(), 2);

        let mut one = row("c", "gamma", now);
        one.waiting = true;
        let mut two = row("d", "delta", now);
        two.waiting = true;
        let sheet = sessions_sheet(&[rows[0].clone(), one, two], now);
        assert_eq!(&*sheet.count, "3 in this folder \u{b7} 2 waits");

        // An empty folder still names itself.
        let sheet = sessions_sheet(&[], now);
        assert_eq!(&*sheet.count, "0 in this folder");
        assert!(sheet.rows.is_empty());
        assert!(sheet.preview.is_empty());
    }

    /// The marks, the detail and the age of the rows read the session.
    #[test]
    fn the_rows_carry_the_marks_and_the_ages() {
        let now = 1_000_000;
        let mut current = row("aaaa1111", "fix the start test", now);
        current.current = true;
        current.live = true;
        let mut waiting = row("bbbb2222", "steer headless", now - 5 * MINUTE);
        waiting.waiting = true;
        let older = row("cccc3333", "panels", now - 3 * HOUR);
        let oldest = row("dddd4444", "", now - 2 * DAY);
        let sheet = sessions_sheet(&[current, waiting, older, oldest], now);

        assert_eq!(&*sheet.rows[0].name, "fix the start test");
        assert_eq!(sheet.rows[0].mark, super::Mark::Current);
        assert_eq!(&*sheet.rows[0].detail, "live");
        assert_eq!(&*sheet.rows[0].right, "now");

        assert_eq!(sheet.rows[1].mark, super::Mark::Waits, "the approval wins");
        assert_eq!(&*sheet.rows[1].detail, "");
        assert_eq!(&*sheet.rows[1].right, "5m");

        assert_eq!(sheet.rows[2].mark, super::Mark::None);
        assert_eq!(&*sheet.rows[2].right, "3h");

        assert_eq!(&*sheet.rows[3].name, "untitled");
        assert_eq!(&*sheet.rows[3].right, "2d");
    }

    /// A session a headless run wrote draws its name dim; a run from the
    /// terminal UI draws it as text.
    #[test]
    fn headless_rows_are_dim_in_the_sheet() {
        let now = 1_000_000;
        let mut printed = row("aaaa1111", "a printed run", now);
        printed.headless = true;
        let sheet = sessions_sheet(
            &[printed, row("bbbb2222", "a run from the UI", now - HOUR)],
            now,
        );
        assert!(sheet.rows[0].dim, "a headless run's row draws dim");
        assert!(!sheet.rows[1].dim, "a run from the UI draws as text");
    }

    /// The pick answers the id of the row the selection stands on.
    #[test]
    fn picked_answers_the_id_of_the_selected_row() {
        let now = 1_000_000;
        let rows = [
            row("aaaa1111", "one", now),
            row("bbbb2222", "two", now - HOUR),
        ];
        let mut sheet = sessions_sheet(&rows, now);
        assert_eq!(picked(&sheet, &rows).as_deref(), Some("aaaa1111"));
        sheet.step(1);
        assert_eq!(picked(&sheet, &rows).as_deref(), Some("bbbb2222"));
        sheet.step(1);
        assert_eq!(picked(&sheet, &rows).as_deref(), Some("bbbb2222"));

        let empty: [SessionRow; 0] = [];
        assert_eq!(picked(&sessions_sheet(&empty, now), &empty), None);

        // A filter that matches nothing picks no session.
        for c in "zzz".chars() {
            sheet.push(c);
        }
        assert_eq!(picked(&sheet, &rows), None);
    }

    /// The preview of a waiting live session names its id, its approval, its
    /// worker and its age, and dims all but the two the user acts on.
    #[test]
    fn the_preview_of_a_waiting_live_session_reads_the_session() {
        let now = 1_000_000;
        let mut session = row("abcdef0123456789", "steer headless", now - 3 * HOUR);
        session.waiting = true;
        session.live = true;
        let lines = preview(&session, now);
        assert_eq!(
            texts(&lines),
            [
                "steer headless",
                "abcdef01 \u{b7} live \u{b7} changed 3h ago",
                "waits for an approval",
            ]
        );
        assert_eq!(dims(&lines), [false, true, false]);
    }

    /// The preview of a session that runs nothing says so, and a session with no
    /// title reads `untitled`.
    #[test]
    fn the_preview_of_an_idle_session_says_not_running() {
        let now = 1_000_000;
        let session = row("0123456789", "", now - 5 * MINUTE);
        let lines = preview(&session, now);
        assert_eq!(
            texts(&lines),
            [
                "untitled",
                "01234567 \u{b7} not running \u{b7} changed 5m ago",
            ]
        );
        // An age under a minute reads `now` and takes no `ago`.
        let fresh = row("0123456789", "", now - 30);
        assert_eq!(
            texts(&preview(&fresh, now)),
            ["untitled", "01234567 \u{b7} not running \u{b7} changed now"]
        );
    }

    /// A reply draws as the transcript draws it: its markdown rendered, the
    /// bold without its stars and a code block on the block's own fill.
    #[test]
    fn session_preview_draws_a_reply_as_markdown() {
        let theme = rich();
        let recent = RecentMessages {
            messages: vec![Recent::Reply(Arc::from(
                "say **bold** and\n\n```\nlet x = 1;\n```",
            ))],
        };
        let rows = preview_rows(&recent, 40, &theme);
        assert!(rows[0].runs.is_empty(), "a blank row sets the reply off");
        let drawn = row_text(&rows[1]);
        assert_eq!(drawn, "say bold and", "{drawn}");
        assert!(
            !rows.iter().any(|row| row_text(row).contains('*')),
            "no stars in the pane"
        );

        let bold = style_of(&rows, "bold");
        assert!(bold.bold, "the words the reply marks bold draw bold");
        assert_eq!(bold.bg, theme.fill(Role::InputFill), "over the pane's fill");
    }

    /// A code block keeps its own fill, so the pane has no cell without one.
    #[test]
    fn session_preview_draws_a_code_block_on_its_own_fill() {
        let theme = rich();
        let recent = RecentMessages {
            messages: vec![Recent::Reply(Arc::from(
                "the text\n\n```\nlet x = 1;\n```\n",
            ))],
        };
        let rows = preview_rows(&recent, 40, &theme);
        assert_eq!(
            style_of(&rows, "text").bg,
            theme.fill(Role::InputFill),
            "a plain row is the pane's fill"
        );
        let code = style_of(&rows, "let x = 1;");
        assert_eq!(code.bg, theme.fill(Role::CodeFill), "the block's own fill");
    }

    /// A user turn starts with `›` in the accent and wraps its words two
    /// columns in, the rows under the first one keeping those columns.
    #[test]
    fn session_preview_draws_a_user_turn_under_its_mark() {
        let theme = rich();
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let recent = RecentMessages {
            messages: vec![Recent::User(Arc::from(text))],
        };
        let rows = preview_rows(&recent, 24, &theme);
        assert!(rows[0].runs.is_empty(), "a blank row sets the turn off");
        let first = &rows[1];
        assert_eq!(first.start, 0);
        assert_eq!(&*first.runs[0].text, super::TURN, "the mark first");
        assert_eq!(
            first.runs[0].style.fg,
            theme.styled(Role::Accent, Emphasis::Bold).fg
        );
        assert!(first.runs[1].style.bold, "the words draw bold");

        let wrapped = &rows[2..];
        assert!(wrapped.len() > 1, "the turn wraps: {} rows", wrapped.len());
        for row in wrapped {
            assert_eq!(row.start, super::TURN_AT, "two columns under the mark");
            assert!(
                !row.runs.iter().any(|run| &*run.text == super::TURN),
                "`›` draws on the first row alone"
            );
        }
        let drawn: Vec<String> = rows.iter().map(row_text).collect();
        let joined = drawn[1..].join(" ");
        assert!(joined.contains(text), "every word draws: {joined}");
    }

    /// A file that moves nothing prints each word as the table has it.
    #[test]
    fn keys_in_force_sessions_words_match_the_table() {
        let (file, _) = crate::keyfile::KeyFile::parse("");
        let labels = file.labels();
        assert_eq!(labels.foot(crate::keyfile::Foot::Sessions), super::FOOTER);
        assert_eq!(labels.foot(crate::keyfile::Foot::SessionsHint), super::HINT);
    }
}
