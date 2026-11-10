//! The Sessions panel: the sessions of the folder, newest first.
//!
//! The panel reads no disk: the app lists the folder's sessions and hands them
//! over with [`SessionsPanel::set_rows`], together with the clock. A row's
//! text and its age are built there, when the sessions or the age unit change,
//! so a draw copies no text and allocates nothing.
//!
//! One page shows at most eight sessions, with the footer row under them when
//! the folder has more: `1–8 of 40` and the two arrows that turn the page. A
//! session with no title reads `untitled`, and one a headless run wrote draws
//! dim.

use std::cell::Cell;
use std::cmp::Reverse;
use std::sync::Arc;

use crate::slot::{Clicked, Component, Panel, Priority, Role, Size, Text, Update};

/// The fold order: the session list is the first panel to fold.
const PRIORITY: Priority = 1;

/// The narrowest sidebar this panel draws in.
const MIN_WIDTH: u16 = 20;

/// The mark of a session whose worker runs now, and of the session of this
/// TUI, which is the current row. The host draws it in the accent role.
const LIVE: &str = "●";

/// The mark of a session that waits for an approval. It wins over [`LIVE`],
/// because an approval is the thing to look at, and it draws in the warn role.
const WAITING: &str = "⏸";

/// The most sessions one page shows. A folder with no more needs no footer.
const PAGE: usize = 8;

/// The word of a session that has no title.
const UNTITLED: &str = "untitled";

/// The arrows of the footer row, and the byte where the second one starts.
const ARROWS: &str = "‹ ›";
const ARROW_SPLIT: usize = "‹ ".len();

/// The columns the two arrows take, right-aligned on the footer row.
const ARROW_COLUMNS: u16 = 3;

/// The seconds of an age unit.
const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// One session of the folder, as the app read it from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The session's id.
    pub id: Arc<str>,
    /// Its title.
    pub title: Arc<str>,
    /// When it last changed, in unix seconds.
    pub updated: u64,
    /// Whether a worker runs it now.
    pub live: bool,
    /// Whether a worker of it waits for an approval, its own here or another
    /// session's in the discovery folder.
    pub waiting: bool,
    /// Whether it is the session of this TUI.
    pub current: bool,
    /// Whether a headless run wrote it, which draws its title dim.
    pub headless: bool,
}

/// One row as the panel draws it: its text and its note built once.
#[derive(Debug)]
struct Line {
    /// The session's id, which finds the row again on the next `set_rows`.
    id: Arc<str>,
    /// The title.
    text: Arc<str>,
    /// The mark before the title, empty when the row has none.
    mark: &'static str,
    /// The role of the mark: the accent for a live session, the warn for one
    /// that waits.
    mark_role: Role,
    /// The age, such as `3m`.
    note: Arc<str>,
    /// Whether it is the session of this TUI.
    selected: bool,
    /// Whether a headless run wrote it, so the title draws dim.
    headless: bool,
    /// What a later `set_rows` compares, so the same rows build no text.
    updated: u64,
    live: bool,
    waiting: bool,
}

/// The row under a page of sessions: the range and the folder's total, and the
/// two arrows that turn the page.
#[derive(Debug)]
struct Footer {
    /// The range and the total, such as `1–8 of 40`, built when either
    /// changes.
    text: Arc<str>,
    /// The first session of the page, from 1.
    first: usize,
    /// The last session of the page, from 1.
    last: usize,
    /// The sessions the folder has.
    total: usize,
    /// The role of `‹`: dim on the first page.
    prev: Role,
    /// The role of `›`: dim on the last page.
    next: Role,
}

/// The Sessions panel.
#[derive(Debug)]
pub struct SessionsPanel {
    title: Text,
    lines: Vec<Line>,
    /// The clock of the last `set_rows`, which the age comparison reads.
    now: u64,
    /// How many sessions wait for an approval, as the header row reads them:
    /// `1 waits`.
    waits: Text,
    /// The index of the first session of the page shown.
    at: usize,
    /// The row under the page, or none while the sessions fit on one page.
    footer: Option<Footer>,
    /// The columns of the last draw, which a click on the footer reads.
    columns: Cell<u16>,
}

impl SessionsPanel {
    /// An empty panel.
    pub fn new() -> SessionsPanel {
        SessionsPanel {
            title: Text::Static("Sessions"),
            lines: Vec::new(),
            now: 0,
            waits: Text::Empty,
            at: 0,
            footer: None,
            columns: Cell::new(0),
        }
    }

    /// The sessions that wait for an approval, as the header row reads them:
    /// `1 waits`, and empty when none does.
    pub fn waits(&self) -> &Text {
        &self.waits
    }

    /// Take the folder's sessions and the clock, newest first, and say whether
    /// the panel draws something new.
    ///
    /// The caller's `Vec` is sorted in place, so a refresh with the same rows
    /// allocates nothing and returns false.
    pub fn set_rows(&mut self, mut rows: Vec<SessionRow>, now: u64) -> bool {
        rows.sort_unstable_by_key(|row| Reverse(row.updated));
        let changed = !self.unchanged(&rows, now);
        self.now = now;
        if !changed {
            return false;
        }
        self.lines.clear();
        let mut waiting = 0;
        for row in &rows {
            if row.waiting {
                waiting += 1;
            }
            let (mark, mark_role) = mark(row);
            let (value, unit) = age(row.updated, now);
            let mut note = value.to_string();
            note.push_str(unit);
            let text: Arc<str> = if row.title.is_empty() {
                Arc::from(UNTITLED)
            } else {
                Arc::clone(&row.title)
            };
            self.lines.push(Line {
                id: Arc::clone(&row.id),
                text,
                mark,
                mark_role,
                note: Arc::from(note),
                selected: row.current,
                headless: row.headless,
                updated: row.updated,
                live: row.live,
                waiting: row.waiting,
            });
        }
        self.waits = if waiting == 0 {
            Text::Empty
        } else {
            Text::Shared(Arc::from(format!("{waiting} waits")))
        };
        self.fit_page();
        true
    }

    /// Turns the page by `delta` pages, keeping it inside the pages the folder
    /// has, and says whether it moved.
    pub fn page(&mut self, delta: i32) -> bool {
        let pages = self.pages();
        if pages <= 1 || delta == 0 {
            return false;
        }
        let last = (pages - 1) * PAGE;
        let step = (PAGE as i32).saturating_mul(delta);
        let at = (self.at as i32).saturating_add(step).clamp(0, last as i32) as usize;
        if at == self.at {
            return false;
        }
        self.at = at;
        self.fit_page();
        true
    }

    /// How many pages the sessions make.
    fn pages(&self) -> usize {
        self.lines.len().div_ceil(PAGE)
    }

    /// Clamps the page to the sessions the panel has, and builds the footer row
    /// again when the range or the total changed.
    fn fit_page(&mut self) {
        let total = self.lines.len();
        if total <= PAGE {
            self.at = 0;
            self.footer = None;
            return;
        }
        let last = (total - 1) / PAGE * PAGE;
        self.at = self.at.min(last);
        let (first, end) = (self.at + 1, (self.at + PAGE).min(total));
        let same = self.footer.as_ref().is_some_and(|footer| {
            footer.first == first && footer.last == end && footer.total == total
        });
        if !same {
            self.footer = Some(Footer {
                text: Arc::from(format!("{first}–{end} of {total}")),
                first,
                last: end,
                total,
                prev: Role::Dim,
                next: Role::Dim,
            });
        }
        let prev = if self.at == 0 { Role::Dim } else { Role::Text };
        let next = if self.at >= last {
            Role::Dim
        } else {
            Role::Text
        };
        if let Some(footer) = &mut self.footer {
            footer.prev = prev;
            footer.next = next;
        }
    }

    /// The page a click at `column` of the footer row asks for: `-1` on `‹`,
    /// `1` on `›`, and none on the rest of the line. The arrows end at the
    /// panel's right edge, where the host puts a note.
    fn arrow(&self, column: u16) -> i32 {
        let width = self.columns.get();
        if width < ARROW_COLUMNS {
            return 0;
        }
        match column.checked_sub(width - ARROW_COLUMNS) {
            Some(0) => -1,
            Some(2) => 1,
            _ => 0,
        }
    }

    /// Whether the panel already shows these rows at this clock: the same rows
    /// in the same order, in the same age unit.
    fn unchanged(&self, rows: &[SessionRow], now: u64) -> bool {
        self.lines.len() == rows.len()
            && self.lines.iter().zip(rows).all(|(line, row)| {
                line.id == row.id
                    && line.updated == row.updated
                    && line.live == row.live
                    && line.waiting == row.waiting
                    && line.selected == row.current
                    && line.headless == row.headless
                    && age(line.updated, self.now) == age(row.updated, now)
            })
    }
}

impl Default for SessionsPanel {
    fn default() -> Self {
        SessionsPanel::new()
    }
}

impl Panel for SessionsPanel {
    fn id(&self) -> &str {
        "sessions"
    }

    fn title(&self) -> &Text {
        &self.title
    }

    fn priority(&self) -> Priority {
        PRIORITY
    }

    fn min_width(&self) -> u16 {
        MIN_WIDTH
    }

    fn update(&mut self, _update: &Update<'_>) -> bool {
        false
    }

    fn draw(&self, size: Size, out: &mut Vec<Component>) {
        self.columns.set(size.width);
        let start = self.at.min(self.lines.len());
        let end = start.saturating_add(PAGE).min(self.lines.len());
        for line in &self.lines[start..end] {
            let item = Component::item_marked(
                &line.text,
                line.mark,
                line.mark_role,
                &line.note,
                line.selected,
            );
            out.push(if line.headless { item.dimmed() } else { item });
        }
        if let Some(footer) = &self.footer {
            out.push(
                Component::item_split(
                    Text::Shared(Arc::clone(&footer.text)),
                    Text::Static(ARROWS),
                    ARROW_SPLIT,
                    footer.prev,
                    footer.next,
                    false,
                )
                .dimmed(),
            );
        }
        // One blank row between the panels, under a list that has rows.
        if !self.lines.is_empty() {
            out.push(Component::text(Text::Static("")));
        }
    }

    fn click(&mut self, component: usize, column: u16) -> Clicked {
        let start = self.at.min(self.lines.len());
        let end = start.saturating_add(PAGE).min(self.lines.len());
        if component < end - start {
            let line = &self.lines[start + component];
            return if line.selected {
                Clicked::None
            } else {
                Clicked::Open(Arc::clone(&line.id))
            };
        }
        if self.footer.is_none() || component != end - start {
            return Clicked::None;
        }
        let delta = self.arrow(column);
        if delta == 0 {
            return Clicked::None;
        }
        self.page(delta);
        Clicked::Redraw
    }
}

/// The mark a row starts with, at most three columns wide, so it reads at the
/// 28 columns of the narrowest sidebar, and the role it draws in. The session
/// of this TUI carries the dot, so the row the user is on always reads with
/// one.
fn mark(row: &SessionRow) -> (&'static str, Role) {
    if row.waiting {
        (WAITING, Role::Warn)
    } else if row.live || row.current {
        (LIVE, Role::Accent)
    } else {
        ("", Role::Dim)
    }
}

/// The number and the unit of an age, such as `3` and `m`. Two ages that share
/// them read the same.
fn age(updated: u64, now: u64) -> (u64, &'static str) {
    let seconds = now.saturating_sub(updated);
    if seconds < MINUTE {
        (seconds, "s")
    } else if seconds < HOUR {
        (seconds / MINUTE, "m")
    } else if seconds < DAY {
        (seconds / HOUR, "h")
    } else {
        (seconds / DAY, "d")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, updated: u64) -> SessionRow {
        SessionRow {
            id: Arc::from(id),
            title: Arc::from(id),
            updated,
            live: false,
            waiting: false,
            current: false,
            headless: false,
        }
    }

    /// A folder of forty sessions, newest first.
    fn forty(now: u64) -> Vec<SessionRow> {
        (0..40)
            .map(|index| row(&format!("s{index}"), now - index as u64 * MINUTE))
            .collect()
    }

    /// The footer row of a draw: the range it reads and the roles of its two
    /// arrows.
    fn footer_of(components: &[Component]) -> Option<(&str, Role, Role)> {
        components.iter().find_map(|component| match component {
            Component::Item { text, note, .. } if note.as_str() == ARROWS => {
                Some((text.as_str(), note.first(), note.second()))
            }
            _ => None,
        })
    }

    /// Where the text of the footer row starts, so two draws that share it read
    /// the same.
    fn footer_pointer(components: &[Component]) -> Option<*const u8> {
        components.iter().find_map(|component| match component {
            Component::Item { text, note, .. } if note.as_str() == ARROWS => {
                Some(text.as_str().as_ptr())
            }
            _ => None,
        })
    }

    /// Whether a row draws its text dim.
    fn dim_of(component: &Component) -> bool {
        match component {
            Component::Item { dim, .. } => *dim,
            other => panic!("{other:?} is not a row"),
        }
    }

    fn drawn(panel: &SessionsPanel) -> Vec<Component> {
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        out
    }

    /// Every component as text, for comparing two draws.
    fn rendered(components: &[Component]) -> Vec<String> {
        components
            .iter()
            .map(|component| format!("{component:?}"))
            .collect()
    }

    /// The text of a list row.
    fn text_of(component: &Component) -> &str {
        match component {
            Component::Item { text, .. } => text.as_str(),
            other => panic!("{other:?} is not a row"),
        }
    }

    /// The note of a list row.
    fn note_of(component: &Component) -> &str {
        match component {
            Component::Item { note, .. } => note.as_str(),
            other => panic!("{other:?} is not a row"),
        }
    }

    /// The mark of a list row and the role it draws in.
    fn mark_of(component: &Component) -> (&str, Role) {
        match component {
            Component::Item { mark, .. } => (mark.text().as_str(), mark.role()),
            other => panic!("{other:?} is not a row"),
        }
    }

    #[test]
    fn newest_session_comes_first_with_its_age() {
        let now = 1_000_000;
        let mut old = row("older", now - 5 * DAY);
        old.waiting = true;
        let mut middle = row("middle", now - 2 * HOUR);
        middle.live = true;
        let mut newest = row("newest", now - 3 * MINUTE);
        newest.current = true;
        let mut panel = SessionsPanel::new();
        assert!(panel.set_rows(vec![old, middle, newest], now));
        let out = drawn(&panel);
        assert_eq!(out.len(), 4, "three rows and the blank one: {out:?}");
        assert_eq!(text_of(&out[0]), "newest");
        assert_eq!(mark_of(&out[0]), ("●", Role::Accent), "this session's dot");
        assert_eq!(note_of(&out[0]), "3m", "the age of the newest row");
        assert!(out[0].is_selected(), "{out:?}");
        assert_eq!(text_of(&out[1]), "middle");
        assert_eq!(mark_of(&out[1]), ("●", Role::Accent), "a live session");
        assert_eq!(note_of(&out[1]), "2h");
        assert!(!out[1].is_selected());
        assert_eq!(text_of(&out[2]), "older");
        assert_eq!(mark_of(&out[2]), ("⏸", Role::Warn), "a waiting session");
        assert_eq!(note_of(&out[2]), "5d");
        assert_eq!(panel.waits().as_str(), "1 waits");
    }

    #[test]
    fn the_same_sessions_again_change_nothing() {
        let now = 1_000_000;
        let rows = || vec![row("a", now - 3 * MINUTE), row("b", now - 2 * HOUR)];
        let mut panel = SessionsPanel::new();
        assert!(panel.set_rows(rows(), now));
        assert!(!panel.set_rows(rows(), now), "the same rows and ages");
        assert!(!panel.set_rows(rows(), now + 5), "the ages read the same");
        // An age that crosses into the next minute redraws its note.
        assert!(panel.set_rows(rows(), now + MINUTE));
        assert!(!panel.set_rows(rows(), now + MINUTE + 1));
    }

    #[test]
    fn an_ignored_update_changes_nothing() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        assert!(panel.set_rows(vec![row("a", now)], now));
        let before = rendered(&drawn(&panel));
        for update in [
            Update::Usage {
                input: 1,
                cached: 0,
                output: 2,
            },
            Update::Streamed { bytes: 8 },
            Update::Busy(true),
            Update::Tick,
        ] {
            assert!(!panel.update(&update), "{update:?}");
        }
        assert_eq!(rendered(&drawn(&panel)), before);
        assert!(!panel.wants_tick(), "the app feeds the ages");
    }

    #[test]
    fn a_second_draw_keeps_its_buffer_and_returns_equal_components() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        panel.set_rows(
            vec![row("a", now - 60), row("b", now - 3_600), row("c", now)],
            now,
        );
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        let capacity = out.capacity();
        let first = rendered(&out);
        for width in [28, 33, 28] {
            out.clear();
            panel.draw(Size::new(width, 12), &mut out);
            assert_eq!(out.capacity(), capacity, "the buffer grew");
            assert_eq!(rendered(&out), first, "the same components drew");
        }
    }

    #[test]
    fn a_waiting_session_is_counted_for_the_header_row() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        assert_eq!(panel.waits().as_str(), "");
        let mut one = row("one", now);
        one.waiting = true;
        let mut two = row("two", now - 60);
        two.waiting = true;
        assert!(panel.set_rows(vec![one, two], now));
        assert_eq!(panel.waits().as_str(), "2 waits");
        assert!(panel.set_rows(vec![row("three", now)], now));
        assert_eq!(panel.waits().as_str(), "");
    }

    #[test]
    fn untitled_reads_untitled() {
        let now = 1_000_000;
        let mut empty = row("aaaa", now);
        empty.title = Arc::from("");
        let mut panel = SessionsPanel::new();
        assert!(panel.set_rows(vec![empty, row("bbbb", now - MINUTE)], now));
        let out = drawn(&panel);
        assert_eq!(text_of(&out[0]), "untitled");
        assert_eq!(text_of(&out[1]), "bbbb");
    }

    #[test]
    fn headless_rows_are_dim() {
        let now = 1_000_000;
        let mut printed = row("aaaa", now);
        printed.headless = true;
        let mut panel = SessionsPanel::new();
        assert!(panel.set_rows(vec![printed, row("bbbb", now - MINUTE)], now));
        let out = drawn(&panel);
        assert!(dim_of(&out[0]), "a headless run's row draws dim");
        assert!(!dim_of(&out[1]), "a run from the TUI draws as text");
        assert_eq!(mark_of(&out[0]), ("", Role::Dim));
    }

    #[test]
    fn forty_sessions_show_a_page_and_its_footer() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        assert!(panel.set_rows(forty(now), now));
        let out = drawn(&panel);
        assert_eq!(out.len(), 10, "eight rows, the footer and the blank");
        assert_eq!(text_of(&out[0]), "s0", "the newest session first");
        assert_eq!(text_of(&out[7]), "s7", "the eighth of the page");
        assert_eq!(
            footer_of(&out),
            Some(("1\u{2013}8 of 40", Role::Dim, Role::Text)),
            "the range, and only the next arrow lit"
        );
        assert!(matches!(&out[9], Component::Text { .. }), "a blank row");
    }

    #[test]
    fn page_moves_and_clamps() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        panel.set_rows(forty(now), now);
        assert!(!panel.page(-1), "the first page has no page before it");
        assert!(!panel.page(0));
        assert!(panel.page(1));
        let out = drawn(&panel);
        assert_eq!(
            text_of(&out[0]),
            "s8",
            "the second page starts at the ninth"
        );
        assert_eq!(
            footer_of(&out),
            Some(("9\u{2013}16 of 40", Role::Text, Role::Text))
        );
        assert!(panel.page(9), "a page past the last one clamps");
        let out = drawn(&panel);
        assert_eq!(text_of(&out[0]), "s32");
        assert_eq!(
            footer_of(&out),
            Some(("33\u{2013}40 of 40", Role::Text, Role::Dim))
        );
        assert!(!panel.page(1), "the last page has no page after it");
        assert!(panel.page(-1));
        assert_eq!(
            footer_of(&drawn(&panel)),
            Some(("25\u{2013}32 of 40", Role::Text, Role::Text))
        );
        assert!(panel.page(-9));
        assert_eq!(
            footer_of(&drawn(&panel)),
            Some(("1\u{2013}8 of 40", Role::Dim, Role::Text))
        );
        assert!(!panel.page(-1));
    }

    #[test]
    fn a_page_is_kept_and_clamped_when_the_rows_change() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        panel.set_rows(forty(now), now);
        assert!(panel.page(1));
        assert!(!panel.set_rows(forty(now), now), "the same rows and ages");
        assert_eq!(
            footer_of(&drawn(&panel)),
            Some(("9\u{2013}16 of 40", Role::Text, Role::Text)),
            "the page stays"
        );
        let ten: Vec<SessionRow> = forty(now).into_iter().take(10).collect();
        assert!(panel.set_rows(ten, now));
        assert_eq!(
            footer_of(&drawn(&panel)),
            Some(("9\u{2013}10 of 10", Role::Text, Role::Dim)),
            "the page clamps to the last one"
        );
    }

    #[test]
    fn eight_sessions_need_no_footer() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        let eight: Vec<SessionRow> = forty(now).into_iter().take(8).collect();
        assert!(panel.set_rows(eight, now));
        let out = drawn(&panel);
        assert_eq!(footer_of(&out), None, "no footer under eight: {out:?}");
        assert_eq!(out.len(), 9, "eight rows and the blank one");
        assert!(!panel.page(1), "there is no page after this one");
        let nine: Vec<SessionRow> = forty(now).into_iter().take(9).collect();
        assert!(panel.set_rows(nine, now));
        assert_eq!(
            footer_of(&drawn(&panel)),
            Some(("1\u{2013}8 of 9", Role::Dim, Role::Text)),
            "the ninth session brings the footer"
        );
    }

    #[test]
    fn a_steady_draw_builds_no_footer_again() {
        let now = 1_000_000;
        let mut panel = SessionsPanel::new();
        panel.set_rows(forty(now), now);
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        let capacity = out.capacity();
        let first = footer_pointer(&out);
        assert!(first.is_some(), "the draw has a footer");
        out.clear();
        panel.draw(Size::new(28, 12), &mut out);
        assert_eq!(out.capacity(), capacity, "the buffer grew");
        assert_eq!(footer_pointer(&out), first, "the footer was built again");
        assert!(!panel.set_rows(forty(now), now), "the same rows and ages");
        out.clear();
        panel.draw(Size::new(28, 12), &mut out);
        assert_eq!(footer_pointer(&out), first, "the same rows rebuilt it");
    }

    #[test]
    fn an_empty_folder_draws_no_row() {
        let panel = SessionsPanel::new();
        assert!(drawn(&panel).is_empty());
    }
}
