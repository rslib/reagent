//! A sheet in place of the text box: the model, level, sessions, login and key
//! help views.
//!
//! A sheet takes the place of the text box, so it is drawn with the box's own
//! accent bar and input fill and the status line stays under it. Its rows are
//! built once, when it opens: a filter only reorders their indices, so typing
//! allocates nothing once the buffers are warm.
//!
//! [`crate::app::paint::sheet`] draws the sheet and reads the fields here.

pub(crate) mod help;
pub(crate) mod level;
pub(crate) mod login;
pub(crate) mod model;
pub(crate) mod sessions;

use std::borrow::Cow;
use std::cell::Cell;
use std::sync::Arc;

use crate::keyfile::{Foot, KeyFile};
use crate::pick::Matcher;
use crate::screen::Style;

/// The shortest a sheet gets: a blank, the title, the filter, a blank, a row,
/// the footer and the blank under it.
const LEAST: u16 = 8;

/// The rows a sheet leaves to the transcript above its tallest height.
const ROOM: u16 = 2;

/// The sheet's share of the main area's rows, in tenths.
const SHARE: u16 = 6;

/// A row's mark before its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mark {
    /// No mark: a plain item.
    #[default]
    None,
    /// The row in use now: `●` in the accent.
    Current,
    /// A row that waits for an answer: `⏸` in the warn color.
    Waits,
}

/// What a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A dim header, never selected.
    Group,
    /// A row a key can pick.
    Item,
    /// A dim line, never selected.
    Note,
    /// An empty row.
    Blank,
}

/// One row, its text built once when the sheet opens.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    /// What the row is.
    pub(crate) kind: Kind,
    /// The mark before its name.
    pub(crate) mark: Mark,
    /// The text a filter matches and the row draws first.
    pub(crate) name: Arc<str>,
    /// The dim text after the name.
    pub(crate) detail: Arc<str>,
    /// The dim text at the right edge of the row.
    pub(crate) right: Arc<str>,
    /// Whether the name draws dim, as a session a headless run wrote does.
    pub(crate) dim: bool,
}

/// One preview line and whether it is dim.
#[derive(Debug)]
pub(crate) struct Line {
    /// The line's text.
    pub(crate) text: Arc<str>,
    /// Whether the line draws dim.
    pub(crate) dim: bool,
}

/// One run of a preview row: its text and the style it draws in.
#[derive(Debug, Clone)]
pub(crate) struct Run {
    /// The text, over the sheet's own fill.
    pub(crate) text: Arc<str>,
    /// The style of its cells.
    pub(crate) style: Style,
}

/// One row of a preview drawn as the transcript draws it: a blank row, a user
/// turn or a line of a reply.
#[derive(Debug, Default)]
pub(crate) struct PreviewRow {
    /// The pane column the first run draws at: the two columns under the `›` of
    /// a user turn's later rows.
    pub(crate) start: u8,
    /// The runs, left to right.
    pub(crate) runs: Vec<Run>,
}

/// A sheet's rows, its filter and its place in them.
pub(crate) struct Sheet {
    /// The word of the title row, in the accent.
    pub(crate) title: &'static str,
    /// The count after the title, in dim.
    pub(crate) count: Arc<str>,
    /// The hint at the right edge of the title row, in dim: a `Cow` because a
    /// keys file may have moved a key it names.
    pub(crate) hint: Cow<'static, str>,
    /// The keys of the footer row, in dim: a `Cow` for the same reason.
    pub(crate) footer: Cow<'static, str>,
    /// Every row, in the order the sheet was built with.
    pub(crate) rows: Vec<Row>,
    /// Whether the groups flow side by side, as the help sheet's do.
    pub(crate) columns: bool,
    /// What the filter row holds.
    pub(crate) filter: String,
    /// The matcher of the filter, kept between keys.
    matcher: Matcher,
    /// The indices of the rows on screen, best first while a filter is set.
    pub(crate) shown: Vec<u32>,
    /// The index in `shown` of the selected row.
    pub(crate) selected: usize,
    /// The index in `shown` of the first row the draw puts on screen. The draw
    /// is the one place that knows how many rows fit, so it keeps the scroll
    /// here through a shared borrow.
    pub(crate) top: Cell<usize>,
    /// The preview lines beside the rows.
    pub(crate) preview: Vec<Line>,
    /// The rows under those lines, drawn as the transcript draws them, as wide
    /// as the pane they were built for.
    pub(crate) preview_rows: Option<Arc<Vec<PreviewRow>>>,
    /// Whether the sheet shows the preview when it is wide enough.
    pub(crate) previews: bool,
    /// What the footer row says instead of its keys, built when a delete armed
    /// a row.
    pub(crate) confirm: Option<Arc<str>>,
}

impl Sheet {
    /// A sheet of `rows` under a title, a count, a hint and a footer.
    ///
    /// The selection starts on the first item.
    pub(crate) fn new(
        title: &'static str,
        count: Arc<str>,
        hint: &'static str,
        footer: &'static str,
        rows: Vec<Row>,
    ) -> Sheet {
        let mut sheet = Sheet {
            title,
            count,
            hint: Cow::Borrowed(hint),
            footer: Cow::Borrowed(footer),
            rows,
            columns: false,
            filter: String::new(),
            matcher: Matcher::new(),
            shown: Vec::new(),
            selected: 0,
            top: Cell::new(0),
            preview: Vec::new(),
            preview_rows: None,
            previews: false,
            confirm: None,
        };
        sheet.refresh();
        sheet
    }

    /// Names the keys a keys file moved in the hint and the footer, from the
    /// words the file printed once when it was read, so a frame builds nothing.
    pub(crate) fn moved(&mut self, file: Option<&KeyFile>, hint: Option<Foot>, footer: Foot) {
        let Some(labels) = file.map(KeyFile::labels) else {
            return;
        };
        if let Some(hint) = hint {
            self.hint = Cow::Owned(labels.foot(hint).to_owned());
        }
        self.footer = Cow::Owned(labels.foot(footer).to_owned());
    }

    /// The sheet the groups of which flow side by side: the help sheet.
    pub(crate) fn with_columns(mut self) -> Sheet {
        self.columns = true;
        self
    }

    /// The sheet that shows the preview beside its rows: the sessions sheet.
    pub(crate) fn with_previews(mut self) -> Sheet {
        self.previews = true;
        self
    }

    /// Adds one char to the filter and refilters.
    pub(crate) fn push(&mut self, c: char) {
        self.filter.push(c);
        self.refresh();
    }

    /// Takes the last char off the filter and refilters.
    pub(crate) fn pop(&mut self) {
        if self.filter.pop().is_some() {
            self.refresh();
        }
    }

    /// What the filter row holds.
    #[cfg(test)]
    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    /// Moves the selection `delta` items, skipping the rows a key never picks,
    /// and stops at the first and the last item.
    pub(crate) fn step(&mut self, delta: i8) {
        if delta == 0 {
            return;
        }
        let forward = delta > 0;
        let mut left = delta.unsigned_abs();
        let mut at = self.selected;
        let mut probe = self.selected;
        while left > 0 {
            probe = if forward {
                if probe + 1 >= self.shown.len() {
                    break;
                }
                probe + 1
            } else {
                if probe == 0 {
                    break;
                }
                probe - 1
            };
            if self.rows[self.shown[probe] as usize].kind == Kind::Item {
                at = probe;
                left -= 1;
            }
        }
        self.selected = at;
    }

    /// Puts the selection on the row named `word`, ignoring ASCII case, and
    /// leaves it on the first item when no row is named that.
    pub(crate) fn select(&mut self, word: &str) {
        self.selected = self
            .shown
            .iter()
            .position(|&index| self.rows[index as usize].name.eq_ignore_ascii_case(word))
            .unwrap_or(self.selected);
        self.top.set(0);
    }

    /// The selected row and its index in the rows given to [`Sheet::new`].
    pub(crate) fn selected(&self) -> Option<(usize, &Row)> {
        let index = *self.shown.get(self.selected)? as usize;
        Some((index, self.rows.get(index)?))
    }

    /// Replaces the preview lines beside the rows.
    pub(crate) fn set_preview(&mut self, lines: Vec<Line>) {
        self.preview = lines;
    }

    /// Replaces the message rows under those lines.
    pub(crate) fn set_preview_rows(&mut self, rows: Option<Arc<Vec<PreviewRow>>>) {
        self.preview_rows = rows;
    }

    /// Puts `filter` on the sheet and refilters, for a sheet built again.
    pub(crate) fn filter_to(&mut self, filter: &str) {
        self.filter.clear();
        self.filter.push_str(filter);
        self.refresh();
    }

    /// Puts the selection on the row at `at` of the rows on screen, or on the
    /// last row when the list got shorter.
    pub(crate) fn select_at(&mut self, at: usize) {
        self.selected = at.min(self.shown.len().saturating_sub(1));
        self.top.set(0);
    }

    /// How many rows the sheet takes of a main area `main_rows` tall: 60% of
    /// them, the shortest sheet, and never more than the area leaves.
    pub(crate) fn height(&self, main_rows: u16) -> u16 {
        let share = (u32::from(main_rows) * u32::from(SHARE) / 10) as u16;
        share.max(LEAST).min(main_rows.saturating_sub(ROOM))
    }

    /// Fills `shown` from the filter and puts the selection back on the first
    /// item, with the rows scrolled to the top.
    fn refresh(&mut self) {
        self.shown.clear();
        if self.filter.is_empty() {
            self.shown.extend(0..self.rows.len() as u32);
        } else {
            self.matcher
                .run(&self.filter, self.rows.iter().map(|row| &*row.name));
            for hit in self.matcher.hits() {
                if self.rows[hit.index as usize].kind == Kind::Item {
                    self.shown.push(hit.index);
                }
            }
        }
        self.selected = self
            .shown
            .iter()
            .position(|&index| self.rows[index as usize].kind == Kind::Item)
            .unwrap_or(0);
        self.top.set(0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Kind, Mark, Row, Sheet};

    /// An item row with no mark and no right-hand text.
    fn item(name: &str) -> Row {
        Row {
            kind: Kind::Item,
            mark: Mark::None,
            name: Arc::from(name),
            detail: Arc::from(""),
            right: Arc::from(""),
            dim: false,
        }
    }

    /// The rows of the tests: two groups, three items, a blank and a note.
    fn sheet() -> Sheet {
        Sheet::new(
            "Model",
            Arc::from("3 models"),
            "provider \u{b7} context",
            "\u{2191}\u{2193} choose \u{b7} esc closes",
            vec![
                Row {
                    kind: Kind::Group,
                    ..item("recent")
                },
                item("alpha"),
                item("beta"),
                Row {
                    kind: Kind::Blank,
                    ..item("")
                },
                Row {
                    kind: Kind::Group,
                    ..item("other")
                },
                item("gamma"),
                Row {
                    kind: Kind::Note,
                    ..item("\u{2026} 2 more")
                },
            ],
        )
    }

    /// The names of the rows on screen, top down.
    fn names(sheet: &Sheet) -> Vec<&str> {
        sheet
            .shown
            .iter()
            .map(|&index| &*sheet.rows[index as usize].name)
            .collect()
    }

    /// The name of the selected row and its index in the rows of `sheet`.
    fn selected(sheet: &Sheet) -> Option<(usize, &str)> {
        sheet.selected().map(|(index, row)| (index, &*row.name))
    }

    /// A filter keeps the items it matches, in the matcher's order, and leaves
    /// the group rows out.
    #[test]
    fn a_filter_shows_only_the_matching_items_with_no_group_rows() {
        let mut filtered = sheet();
        for c in "gam".chars() {
            filtered.push(c);
        }
        assert_eq!(filtered.filter(), "gam");
        assert_eq!(names(&filtered), ["gamma"]);

        // A query only a group row holds matches nothing: no key lands on a
        // group.
        let mut group = sheet();
        for c in "oth".chars() {
            group.push(c);
        }
        assert!(group.shown.is_empty());
        assert!(group.selected().is_none());
    }

    /// An arrow skips the rows a key never picks and stops at the first and the
    /// last item.
    #[test]
    fn step_skips_group_note_and_blank_rows_and_stops_at_the_ends() {
        let mut sheet = sheet();
        assert_eq!(selected(&sheet), Some((1, "alpha")));

        sheet.step(1);
        assert_eq!(selected(&sheet), Some((2, "beta")));
        sheet.step(1);
        assert_eq!(selected(&sheet), Some((5, "gamma")));
        // The note under the last item is not a place the pick goes.
        sheet.step(1);
        assert_eq!(selected(&sheet), Some((5, "gamma")));

        sheet.step(-1);
        assert_eq!(selected(&sheet), Some((2, "beta")));
        // A page up runs off the top and stops on the first item.
        sheet.step(-10);
        assert_eq!(selected(&sheet), Some((1, "alpha")));
    }

    /// Every filter change puts the selection back on the first item of the new
    /// list.
    #[test]
    fn a_filter_change_resets_the_selection_to_the_first_item() {
        let mut sheet = sheet();
        sheet.step(1);
        assert_eq!(sheet.selected, 2);

        sheet.push('b');
        assert_eq!(sheet.selected, 0);
        assert_eq!(selected(&sheet), Some((2, "beta")));

        sheet.pop();
        assert_eq!(sheet.filter(), "");
        assert_eq!(selected(&sheet), Some((1, "alpha")));
    }

    /// The sheet takes 60% of the main area's rows, between its shortest height
    /// and the two rows the area keeps.
    #[test]
    fn the_height_is_sixty_percent_of_the_main_rows_within_its_bounds() {
        let sheet = sheet();
        assert_eq!(sheet.height(40), 24);
        assert_eq!(sheet.height(20), 12);
        assert_eq!(sheet.height(60), 36);
        // Too short for 60%: the shortest a sheet gets.
        assert_eq!(sheet.height(12), 8);
        // Too short even for that: the main area keeps two rows.
        assert_eq!(sheet.height(8), 6);
        for main in 1..80u16 {
            let height = sheet.height(main);
            assert!(
                height <= main.saturating_sub(2),
                "{main} rows gave {height}"
            );
            if main.saturating_sub(2) >= 8 {
                assert!(height >= 8, "{main} rows gave {height}");
            }
        }
    }

    /// A second filter reuses the buffers the first one filled: the filter, the
    /// matcher and the rows on screen keep their capacity.
    #[test]
    fn a_second_filter_keeps_the_capacities() {
        let mut sheet = sheet();
        for c in "alp".chars() {
            sheet.push(c);
        }
        let shown = sheet.shown.capacity();
        let filter = sheet.filter.capacity();
        let matcher = sheet.matcher.capacity();
        assert_ne!(shown, 0);
        assert_ne!(matcher.0, 0);

        for _ in 0..3 {
            sheet.pop();
        }
        for c in "gam".chars() {
            sheet.push(c);
        }
        assert_eq!(names(&sheet), ["gamma"]);
        assert_eq!(sheet.shown.capacity(), shown);
        assert_eq!(sheet.filter.capacity(), filter);
        assert_eq!(sheet.matcher.capacity(), matcher);
    }
}
