//! The login sheet.
//!
//! A login runs in the host and sends its steps here: the sheet marks the
//! provider being logged in and lists the lines its steps left. No step holds a
//! key or a token, so the sheet never draws a secret.

use std::sync::Arc;

use super::{Kind, Line, Mark, Row, Sheet};
use crate::host::LoginStep;

/// The title of the sheet.
const TITLE: &str = "Log in";

/// The hint at the right edge of the title row.
const HINT: &str = "enter starts a login";

/// The footer while no login runs.
const FOOTER: &str = "\u{2191}\u{2193} choose \u{b7} enter logs in \u{b7} esc back to your draft";

/// The footer while a login runs.
const WAITING: &str = "esc stops waiting \u{b7} the login goes on in the browser";

/// The login state: the providers, the one being logged in, and the lines its
/// steps left.
pub(crate) struct Login {
    /// The providers a login can open, in the order the sheet lists them.
    providers: Vec<&'static str>,
    /// The provider being logged in.
    running: Option<&'static str>,
    /// The lines the steps of the running login left, oldest first.
    lines: Vec<Line>,
    /// Whether the last login finished, and whether it worked.
    done: Option<bool>,
}

impl Login {
    /// The login over `providers`, with no login running.
    pub(crate) fn new(providers: &[&'static str]) -> Login {
        Login {
            providers: providers.to_vec(),
            running: None,
            lines: Vec::new(),
            done: None,
        }
    }

    /// The sheet for the state now; the app builds it again after each step.
    pub(crate) fn sheet(&self) -> Sheet {
        let mut rows = Vec::with_capacity(self.providers.len() + self.lines.len() + 1);
        for provider in &self.providers {
            let running = self.running == Some(*provider);
            rows.push(Row {
                kind: Kind::Item,
                mark: if running { Mark::Current } else { Mark::None },
                name: Arc::from(*provider),
                detail: Arc::from(""),
                right: Arc::from(""),
                dim: false,
            });
        }
        if self.running.is_some() {
            rows.push(row(Kind::Blank, ""));
            for line in &self.lines {
                rows.push(row(Kind::Note, &line.text));
            }
        }
        let footer = if self.running.is_some() {
            WAITING
        } else {
            FOOTER
        };
        let count = format!("{} providers", self.providers.len());
        Sheet::new(TITLE, Arc::from(count), HINT, footer, rows)
    }

    /// The provider of the selected row, which the app starts a login for.
    pub(crate) fn picked(&self, sheet: &Sheet) -> Option<&'static str> {
        let (index, row) = sheet.selected()?;
        if row.kind != Kind::Item {
            return None;
        }
        self.providers.get(index).copied()
    }

    /// Starts a login for `provider` and forgets the lines of the last one.
    pub(crate) fn start(&mut self, provider: &'static str) {
        self.running = Some(provider);
        self.lines.clear();
        self.done = None;
    }

    /// Takes one step of the running login.
    pub(crate) fn step(&mut self, step: &LoginStep) {
        let (text, dim) = match step {
            LoginStep::Open { url } => (format!("Open: {url}"), false),
            LoginStep::Code { page, code } => (format!("Go to {page} and enter {code}"), false),
            LoginStep::Done => (
                format!("Logged in to {}", self.running.unwrap_or_default()),
                true,
            ),
            LoginStep::Failed(reason) => (format!("Failed: {reason}"), true),
        };
        self.done = match step {
            LoginStep::Done => Some(true),
            LoginStep::Failed(_) => Some(false),
            LoginStep::Open { .. } | LoginStep::Code { .. } => self.done,
        };
        self.lines.push(Line {
            text: Arc::from(text),
            dim,
        });
    }

    /// Whether the last login finished, and whether it worked.
    pub(crate) fn finished(&self) -> Option<bool> {
        self.done
    }
}

/// A row of `kind` named `name`, with no mark and no text after the name.
fn row(kind: Kind, name: &str) -> Row {
    Row {
        kind,
        mark: Mark::None,
        name: Arc::from(name),
        detail: Arc::from(""),
        right: Arc::from(""),
        dim: false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Login;
    use crate::app::sheet::{Kind, Mark, Sheet};
    use crate::host::LoginStep;

    /// The providers of the tests.
    const PROVIDERS: [&str; 2] = ["anthropic", "openai"];

    /// A login over [`PROVIDERS`] with nothing running.
    fn login() -> Login {
        Login::new(&PROVIDERS)
    }

    /// The names of every row, top down.
    fn names(sheet: &Sheet) -> Vec<String> {
        sheet.rows.iter().map(|row| row.name.to_string()).collect()
    }

    /// The names of the providers the sheet lists.
    #[test]
    fn the_sheet_lists_a_row_per_provider() {
        let sheet = login().sheet();
        assert_eq!(sheet.title, "Log in");
        assert_eq!(&*sheet.count, "2 providers");
        assert_eq!(sheet.hint, "enter starts a login");
        assert_eq!(
            sheet.footer,
            "\u{2191}\u{2193} choose \u{b7} enter logs in \u{b7} esc back to your draft"
        );
        assert_eq!(names(&sheet), PROVIDERS);
        assert!(sheet.rows.iter().all(|row| row.kind == Kind::Item));
        assert!(sheet.rows.iter().all(|row| row.mark == Mark::None));
    }

    /// The steps of a login leave their lines under the providers, the running
    /// provider is marked, and `Done` finishes it.
    #[test]
    fn start_then_open_code_and_done_leaves_the_note_lines() {
        let mut login = login();
        login.start("anthropic");
        let sheet = login.sheet();
        assert_eq!(
            sheet.footer,
            "esc stops waiting \u{b7} the login goes on in the browser"
        );
        assert_eq!(sheet.rows[0].mark, Mark::Current);
        assert_eq!(sheet.rows[1].mark, Mark::None);
        assert_eq!(sheet.rows[2].kind, Kind::Blank);

        login.step(&LoginStep::Open {
            url: Arc::from("https://example.com/device"),
        });
        login.step(&LoginStep::Code {
            page: Arc::from("https://example.com/device"),
            code: Arc::from("ABCD-1234"),
        });
        assert_eq!(login.finished(), None);
        login.step(&LoginStep::Done);
        assert_eq!(login.finished(), Some(true));

        let sheet = login.sheet();
        assert_eq!(
            names(&sheet),
            [
                "anthropic",
                "openai",
                "",
                "Open: https://example.com/device",
                "Go to https://example.com/device and enter ABCD-1234",
                "Logged in to anthropic",
            ]
        );
        assert_eq!(sheet.rows[0].mark, Mark::Current);
        // The blank and the lines are never a place a key picks.
        assert!(sheet.rows[2..].iter().all(|row| row.kind != Kind::Item));
    }

    /// A failed login says why and finishes as a failure.
    #[test]
    fn a_failed_step_leaves_the_failure_line() {
        let mut login = login();
        login.start("openai");
        login.step(&LoginStep::Failed(Arc::from("the browser never answered")));
        assert_eq!(login.finished(), Some(false));
        let sheet = login.sheet();
        assert_eq!(
            &*sheet.rows.last().expect("a note row").name,
            "Failed: the browser never answered"
        );
        assert_eq!(sheet.rows[0].mark, Mark::None);
        assert_eq!(sheet.rows[1].mark, Mark::Current);
    }

    /// A second login forgets the lines and the end of the first.
    #[test]
    fn a_second_start_forgets_the_last_login() {
        let mut login = login();
        login.start("anthropic");
        login.step(&LoginStep::Failed(Arc::from("no")));
        login.start("openai");
        assert_eq!(login.finished(), None);
        assert_eq!(names(&login.sheet()).len(), 3);
    }

    /// The pick is the provider of the selected row, and never a note.
    #[test]
    fn picked_is_the_provider_of_the_selected_row() {
        let mut login = login();
        let sheet = login.sheet();
        assert_eq!(login.picked(&sheet), Some("anthropic"));

        let mut moved = login.sheet();
        moved.step(1);
        assert_eq!(login.picked(&moved), Some("openai"));

        // The lines of a running login are notes: a key never lands on one.
        login.start("openai");
        login.step(&LoginStep::Done);
        let mut sheet = login.sheet();
        sheet.step(10);
        assert_eq!(login.picked(&sheet), Some("openai"));

        let empty = Login::new(&[]);
        assert_eq!(empty.picked(&empty.sheet()), None);
    }
}
