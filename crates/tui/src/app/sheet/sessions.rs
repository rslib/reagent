//! The sessions sheet.
//!
//! The sheet lists the sessions of the folder, newest first, and shows the
//! preview of the selected one beside the rows at a hundred columns or more.
//! The rows and the preview lines are built once, when the sheet opens, so a
//! key moves the selection without building a string.

use std::sync::Arc;

use crate::panels::SessionRow;

use super::{Kind, Line, Mark, Row, Sheet};

/// The word of the title row.
const TITLE: &str = "Sessions";

/// The hint at the right edge of the title row.
const HINT: &str = "enter opens";

/// The keys of the footer row.
const FOOTER: &str = "\u{2191}\u{2193} choose \u{b7} enter opens \u{b7} esc back to your draft";

/// The name of a session that has no title.
const UNTITLED: &str = "untitled";

/// The detail of a session a worker runs now.
const LIVE: &str = "live";

/// The preview line of a session that waits for an approval.
const WAITS: &str = "waits for an approval";

/// The preview lines of a session a worker runs, and of one it does not.
const RUNNING: &str = "live, a worker runs it";
const IDLE: &str = "not running; enter starts it";

/// The seconds of an age unit.
const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

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

/// The preview lines of one session.
pub(crate) fn preview(row: &SessionRow, now: u64) -> Vec<Line> {
    let seconds = now.saturating_sub(row.updated);
    let age = age(seconds);
    let changed = if seconds < MINUTE {
        format!("changed {age}")
    } else {
        format!("changed {age} ago")
    };
    let id: String = row.id.chars().take(8).collect();
    let mut lines = Vec::with_capacity(5);
    lines.push(Line {
        text: Arc::from(name(row)),
        dim: false,
    });
    lines.push(Line {
        text: Arc::from(format!("id {id}")),
        dim: true,
    });
    if row.waiting {
        lines.push(Line {
            text: Arc::from(WAITS),
            dim: false,
        });
    }
    lines.push(Line {
        text: Arc::from(if row.live { RUNNING } else { IDLE }),
        dim: true,
    });
    lines.push(Line {
        text: Arc::from(changed),
        dim: true,
    });
    lines
}

/// The id of the selected session, for opening it.
pub(crate) fn picked(sheet: &Sheet, rows: &[SessionRow]) -> Option<Arc<str>> {
    let (index, _) = sheet.selected()?;
    rows.get(index).map(|row| Arc::clone(&row.id))
}

/// The name of a session: its title, or `untitled` when it has none.
fn name(row: &SessionRow) -> &str {
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

    use super::{DAY, HOUR, MINUTE, picked, preview, sessions_sheet};

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
            "\u{2191}\u{2193} choose \u{b7} enter opens \u{b7} esc back to your draft"
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
                "id abcdef01",
                "waits for an approval",
                "live, a worker runs it",
                "changed 3h ago",
            ]
        );
        assert_eq!(dims(&lines), [false, true, false, true, true]);
    }

    /// The preview of a session that runs nothing says the enter starts it, and
    /// a session with no title reads `untitled`.
    #[test]
    fn the_preview_of_an_idle_session_says_enter_starts_it() {
        let now = 1_000_000;
        let session = row("0123456789", "", now - 5 * MINUTE);
        let lines = preview(&session, now);
        assert_eq!(
            texts(&lines),
            [
                "untitled",
                "id 01234567",
                "not running; enter starts it",
                "changed 5m ago",
            ]
        );
        // An age under a minute reads `now` and takes no `ago`.
        let fresh = row("0123456789", "", now - 30);
        assert_eq!(
            texts(&preview(&fresh, now)),
            [
                "untitled",
                "id 01234567",
                "not running; enter starts it",
                "changed now"
            ]
        );
    }
}
