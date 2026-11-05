//! Styled lines: what the host turns components into.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{Role, Text};

/// The ellipsis a line ends with when its text does not fit.
const ELLIPSIS: &str = "…";

/// The columns the ellipsis takes.
const ELLIPSIS_COLUMNS: u16 = 1;

/// One run of a line: a slice of a [`Text`] in one role.
///
/// The slice is a byte range, so a run that shows part of a bar or part of a
/// panel's text shares that text's buffer: no copy, no allocation.
#[derive(Debug, Clone)]
pub struct Span {
    text: Text,
    start: u32,
    end: u32,
    role: Role,
}

impl Span {
    /// The characters this run shows.
    pub fn text(&self) -> &str {
        &self.text.as_str()[self.start as usize..self.end as usize]
    }

    /// The role that picks its style.
    pub fn role(&self) -> Role {
        self.role
    }

    /// How many columns it takes.
    pub fn width(&self) -> u16 {
        columns(self.text())
    }
}

/// One line of a panel: a range of the spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Line {
    start: u32,
    end: u32,
}

impl Line {
    /// How many runs the line has.
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    /// True when the line draws nothing.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// The lines of one sidebar draw, and the runs they are made of.
///
/// One buffer is reused across frames: [`Lines::clear`] keeps every capacity,
/// so a steady draw allocates nothing.
#[derive(Debug, Default)]
pub struct Lines {
    spans: Vec<Span>,
    lines: Vec<Line>,
}

impl Lines {
    /// An empty buffer.
    pub fn new() -> Self {
        Lines::default()
    }

    /// Empty both buffers, keeping their capacity.
    pub fn clear(&mut self) {
        self.spans.clear();
        self.lines.clear();
    }

    /// The lines.
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Every run, line by line.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// The runs of one line; an index past the last line reads empty.
    pub fn line(&self, index: usize) -> &[Span] {
        match self.lines.get(index) {
            Some(line) => &self.spans[line.start as usize..line.end as usize],
            None => &[],
        }
    }

    /// How many lines there are.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True when nothing draws.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The lines as runs each, for a draw loop.
    pub fn iter(&self) -> impl Iterator<Item = &[Span]> {
        self.lines
            .iter()
            .map(|line| &self.spans[line.start as usize..line.end as usize])
    }

    /// Capacity of the run vector, for the steady-draw allocation guard.
    pub fn span_capacity(&self) -> usize {
        self.spans.capacity()
    }

    /// Capacity of the line vector, for the steady-draw allocation guard.
    pub fn line_capacity(&self) -> usize {
        self.lines.capacity()
    }

    /// Where the next line's runs start.
    pub(crate) fn open(&self) -> u32 {
        self.spans.len() as u32
    }

    /// Close the line whose runs started at `start`.
    pub(crate) fn close(&mut self, start: u32) {
        self.lines.push(Line {
            start,
            end: self.spans.len() as u32,
        });
    }

    /// Add a whole text in one role, and say how many columns it took.
    pub(crate) fn push(&mut self, text: &Text, role: Role) -> u16 {
        let end = text.len() as u32;
        self.push_slice(text, 0, end, role)
    }

    /// Add as much of `text` as fits in `columns` columns, with an ellipsis
    /// when it does not fit, and say how many columns it took. No columns of
    /// room adds nothing, not even the ellipsis: a line never draws wider than
    /// the width it was given.
    pub(crate) fn push_cut(&mut self, text: &Text, role: Role, columns: u16) -> u16 {
        if columns == 0 {
            return 0;
        }
        let (end, cut) = fit(text.as_str(), columns);
        if end == 0 && !cut {
            return 0;
        }
        let used = self.push_slice(text, 0, end as u32, role);
        if cut {
            let ellipsis = Text::Static(ELLIPSIS);
            self.push_slice(&ellipsis, 0, ELLIPSIS.len() as u32, role);
            used + ELLIPSIS_COLUMNS
        } else {
            used
        }
    }

    /// Add the bytes `start..end` of `text` in one role, and say how many
    /// columns they took.
    pub(crate) fn push_slice(&mut self, text: &Text, start: u32, end: u32, role: Role) -> u16 {
        if end <= start {
            return 0;
        }
        let span = Span {
            text: text.clone(),
            start,
            end,
            role,
        };
        let width = span.width();
        self.spans.push(span);
        width
    }
}

/// The columns a string takes on screen.
pub(crate) fn columns(text: &str) -> u16 {
    UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}

/// The byte index where `text` must stop to fit `columns` columns, and whether
/// it had to be cut. `columns` is at least one: [`Lines::push_cut`] takes the
/// no room case before it gets here.
fn fit(text: &str, columns: u16) -> (usize, bool) {
    let columns = columns as usize;
    if UnicodeWidthStr::width(text) <= columns {
        return (text.len(), false);
    }
    if columns <= ELLIPSIS_COLUMNS as usize {
        return (0, true);
    }
    let room = columns - ELLIPSIS_COLUMNS as usize;
    let mut width = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let extra = UnicodeWidthChar::width(ch).unwrap_or(0);
        if extra == 0 {
            // A combining mark belongs to the character before it.
            end = index + ch.len_utf8();
            continue;
        }
        if width + extra > room {
            break;
        }
        width += extra;
        end = index + ch.len_utf8();
    }
    (end, true)
}

/// A run of one repeated character, grown to the widest line drawn and then
/// sliced: a bar's fill, its empty part, and the padding of a note share it.
#[derive(Debug)]
pub(crate) struct Fill {
    text: Text,
    ch: char,
    byte: usize,
    columns: usize,
}

impl Fill {
    /// A fill of one character, one column wide.
    pub(crate) fn new(ch: char) -> Fill {
        let mut fill = Fill {
            text: Text::Empty,
            ch,
            byte: ch.len_utf8(),
            columns: 0,
        };
        fill.fit(1);
        fill
    }

    /// Grow to `columns` columns of the character.
    pub(crate) fn fit(&mut self, columns: u16) {
        let columns = (columns as usize).max(1);
        if columns <= self.columns {
            return;
        }
        let mut text = String::with_capacity(columns * self.byte);
        for _ in 0..columns {
            text.push(self.ch);
        }
        self.columns = columns;
        self.text = Text::Shared(text.into());
    }

    /// Add `columns` columns of the fill in `role`.
    pub(crate) fn push(&self, lines: &mut Lines, columns: u16, role: Role) {
        lines.push_slice(&self.text, 0, self.end(columns), role);
    }

    /// The bytes of `columns` columns.
    pub(crate) fn end(&self, columns: u16) -> u32 {
        let columns = (columns as usize).min(self.columns);
        (columns * self.byte) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_a_range_of_runs() {
        let mut lines = Lines::new();
        let start = lines.open();
        let title = Text::Static("Usage");
        lines.push(&title, Role::Title);
        let value = Text::Static("50k / 200k");
        lines.push(&value, Role::Accent);
        lines.close(start);
        assert_eq!(lines.len(), 1);
        let spans = lines.line(0);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text(), "Usage");
        assert_eq!(spans[0].role(), Role::Title);
        assert_eq!(spans[1].text(), "50k / 200k");
        assert_eq!(spans[1].role(), Role::Accent);
        assert!(lines.line(7).is_empty(), "a line past the last reads empty");
    }

    #[test]
    fn a_cut_text_ends_with_an_ellipsis() {
        let mut lines = Lines::new();
        let text = Text::Static("a long session title");
        let start = lines.open();
        let used = lines.push_cut(&text, Role::Text, 10);
        lines.close(start);
        assert_eq!(used, 10);
        let spans = lines.line(0);
        assert_eq!(spans[0].text(), "a long se");
        assert_eq!(spans[1].text(), "…");
    }

    #[test]
    fn a_text_that_fits_is_not_cut() {
        let mut lines = Lines::new();
        let text = Text::Static("short");
        let start = lines.open();
        let used = lines.push_cut(&text, Role::Text, 10);
        lines.close(start);
        assert_eq!(used, 5);
        assert_eq!(lines.line(0).len(), 1);
        assert_eq!(lines.line(0)[0].text(), "short");
    }

    /// A line never draws wider than the width it was given, so a cut with no
    /// room writes nothing at all, not even the ellipsis (CHG-j9kb5g review).
    #[test]
    fn a_cut_with_no_room_writes_nothing() {
        let mut lines = Lines::new();
        let text = Text::Static("a long session title");
        let start = lines.open();
        let used = lines.push_cut(&text, Role::Text, 0);
        lines.close(start);
        assert_eq!(used, 0);
        assert!(
            lines.line(0).is_empty(),
            "no room takes no ellipsis either: {:?}",
            lines.line(0)[0].text()
        );
    }

    #[test]
    fn a_fill_grows_once_and_slices() {
        let mut fill = Fill::new('█');
        fill.fit(40);
        let mut lines = Lines::new();
        let start = lines.open();
        lines.push_slice(&fill.text, 0, fill.end(3), Role::Accent);
        lines.close(start);
        assert_eq!(lines.line(0)[0].text(), "███");
        assert_eq!(fill.end(3), 9);
        assert_eq!(fill.end(400), 120);
    }

    #[test]
    fn a_wide_character_is_cut_before_it_overflows() {
        let mut lines = Lines::new();
        let text = Text::Static("日本語のタイトル");
        let start = lines.open();
        let used = lines.push_cut(&text, Role::Text, 5);
        lines.close(start);
        assert!(used <= 5);
        for span in lines.line(0) {
            assert!(span.width() <= 5);
        }
    }
}
