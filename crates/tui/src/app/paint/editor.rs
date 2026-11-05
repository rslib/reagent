//! The text box: the editor's rows over the input fill, the accent bar, the
//! footer and the frame's one cursor.
//!
//! The box is the width of the main area whatever the text holds: an accent bar
//! on the left, two cells of padding, the text, a padding row above and below it
//! and a footer row. Every style it draws is resolved once a frame from the
//! theme over the input fill's background (see [`Theme::fill`]), so the box
//! reads as one band in rich mode and as no band at all in terminal mode, where
//! the bar and the weights carry the look.
//!
//! It grows with the text up to a third of the terminal's rows and scrolls
//! inside past that, keeping the edit point in the window. It paints no cursor
//! cell: the frame ends by moving the terminal's own cursor to the edit point
//! (see [`Screen::set_cursor`]), and hides it while the approval card waits.
//!
//! A long paste is one [chip](crate::editor::Paste) in the text: folded, it draws
//! inline as `≡ <first line> … +<lines>` over the code fill, and the one the edit
//! point holds carries an accent underline and a hint row under the text; opened,
//! its bytes are editor text behind a thin accent `│` gutter over a fill a step
//! darker than the box. The chip is drawn from the ranges of its one buffer, so a
//! frame copies none of the paste, and a steady frame allocates nothing.

use unicode_width::UnicodeWidthStr;

use crate::editor::{CHIP, CHIP_MORE, MARK_BYTES, Paste};
use crate::screen::{Cell, Color, Screen, Style};
use crate::slot::{Component, Role as Line, Size, Text};
use crate::theme::{self, Emphasis, Role, Theme};

use super::super::App;
use super::super::list::{List, Source};
use super::list::{self, ListInks, draw_list};
use super::sheet::{self, SheetInks, draw_sheet};

/// The columns of the accent bar on the left of the box.
const BAR: u16 = 1;
/// The blank cells between the bar and the text.
const PAD: u16 = 2;
/// The glyph of the bar: a quarter of a block, full height.
const BAR_GLYPH: &str = "▎";
/// The rows the box draws around the text: a padding row above it, one below it
/// and the footer.
const CHROME: u16 = 3;
/// The empty row between the box and the status line.
const GAP: u16 = 1;
/// The empty row between the transcript and the box.
const TOP: u16 = 1;
/// The tallest the box gets: a third of the terminal's rows.
const THIRD: u16 = 3;
/// The id of the panel that names the model and its settings.
const CONTEXT: &str = "context";
/// What a model the panel does not know reads as.
const NONE: &str = "-";
/// The receiver the footer names.
const RECEIVER: &str = "reagent";
/// The placeholder of an idle empty box.
const PLACEHOLDER: &str = "Ask anything · / commands · @ files";
/// The placeholder of an empty box while a turn runs.
const PLACEHOLDER_BUSY: &str = "Steer the run · alt+enter queues for the next turn";
/// The keys of the footer while a turn runs.
const KEYS_BUSY: &[(&str, &str)] = &[("enter", "steer"), ("alt+enter", "queue")];
/// The keys of the footer while no turn runs.
const KEYS_IDLE: &[(&str, &str)] = &[("enter", "send"), ("alt+enter", "queue")];
/// The gutter an open chip draws beside its lines.
const GUTTER: &str = "│ ";
/// The columns the gutter takes in front of an open chip's text.
const GUTTER_WIDTH: u16 = 2;
/// The hint under the text while the edit point holds a folded chip.
const HINT_PASTE: &str = "tab or a click opens the paste to edit · backspace removes it";

/// The styles one frame of the box draws with, all over the input fill.
#[derive(Debug, Clone, Copy)]
struct Pen {
    /// The bar on the left.
    bar: u32,
    /// The input text and the blank cells of the box.
    text: u32,
    /// The placeholder and the word after a key.
    dim: u32,
    /// The receiver's name.
    accent: u32,
    /// The model.
    bold: u32,
    /// The first line of a folded chip, over the code fill.
    pasted: u32,
    /// The `≡` a folded chip opens with.
    pasted_mark: u32,
    /// The `… +N` a folded chip ends with.
    pasted_count: u32,
    /// The text of an open chip, over the chip's fill.
    open: u32,
    /// The `│` of an open chip.
    open_mark: u32,
    /// The styles of the folded chip the edit point holds, underlined; `None`
    /// while no chip is held, so a steady box interns nothing for it.
    held: Option<Held>,
}

/// The three styles one folded chip draws with while the edit point holds it:
/// the same roles over the code fill, underlined in the accent.
#[derive(Debug, Clone, Copy)]
struct Held {
    /// The `≡`.
    mark: u32,
    /// The first line.
    line: u32,
    /// The `… +N`.
    count: u32,
}

impl App {
    /// How many rows the text box needs: the rows of text, a padding row above
    /// and below them, the footer, the hint row a held chip adds and the empty
    /// row under the box.
    pub(crate) fn editor_height(&mut self) -> u16 {
        if let Some(open) = self.sheet.as_ref() {
            // Inline mode has no main area of its own: the sheet grows into the
            // rows the terminal leaves above the status line.
            let rows = self
                .main
                .height
                .max(self.screen.term_rows().saturating_sub(1));
            return open.sheet.height(rows);
        }
        if self.editor_dirty {
            let columns = self.text_width().saturating_sub(BAR + PAD).max(1);
            let most = self.text_rows_max();
            // A search holds the first of those rows for its prompt, so the text
            // has one row fewer: the box is the rows it shows, the prompt
            // included, and no blank row is left under it.
            let prompt = u16::from(self.editor.searching());
            let text = self
                .editor
                .layout(columns, most.saturating_sub(prompt).max(1))
                .rows()
                .len() as u16;
            self.editor_rows = (text + prompt).clamp(1, most) + self.box_chrome() + TOP + GAP;
            self.editor_dirty = false;
        }
        self.editor_rows
    }

    /// The rows the box draws around the text: a padding row above and below it,
    /// the footer, and the hint row a folded chip at the edit point adds.
    fn box_chrome(&self) -> u16 {
        CHROME + u16::from(self.holds_folded_paste())
    }

    /// Whether the edit point holds a folded chip: its hint row shows and the
    /// chip draws underlined.
    fn holds_folded_paste(&self) -> bool {
        self.editor
            .selected_paste()
            .is_some_and(|index| !self.editor.pastes()[index].is_open())
    }

    /// The most text rows the box shows: a third of the terminal's rows, less
    /// the rows the box draws around the text.
    fn text_rows_max(&self) -> u16 {
        let chrome = self.box_chrome();
        (self.screen.term_rows() / THIRD).max(chrome + 1) - chrome
    }

    /// Draws the text box from row `y` down in `height` rows, with the frame's
    /// cursor on the edit point, and answers the rows it took, the empty rows
    /// above and under the box included.
    pub(crate) fn paint_editor(&mut self, y: u16, height: u16, _reverse: u32) -> u16 {
        let width = self.text_width().max(1);
        if self.sheet.is_some() {
            return self.paint_sheet(y, height, width);
        }
        // The empty row that keeps the transcript's last row off the box. Like
        // the row under the box, it keeps a style with no fill of its own.
        let spacer = self.style(Role::Dim, Emphasis::Plain);
        blank_row(&mut self.screen, y, 0, width, spacer);
        let (y, given) = (y + TOP, height);
        let height = height.saturating_sub(TOP);
        // The hint row shows only when the rows the box was given carry it.
        let hint = self.holds_folded_paste() && height >= CHROME + GAP + 2;
        let chrome = CHROME + u16::from(hint);
        let shown = height
            .saturating_sub(chrome + GAP)
            .clamp(1, self.text_rows_max());
        let pen = self.box_pen();
        // A search holds the first text row for its prompt, and draws the match
        // on the rows under it.
        let prompt = u16::from(self.editor.searching());
        let mut rows = std::mem::take(&mut self.rows);
        let (cursor_row, cursor_col) = {
            let columns = width.saturating_sub(BAR + PAD).max(1);
            let layout = self
                .editor
                .layout(columns, shown.saturating_sub(prompt).max(1));
            rows.clear();
            // The prompt row takes the first cell of the click map, which no
            // chip reaches.
            rows.extend(std::iter::repeat_n((0, 0), usize::from(prompt)));
            rows.extend(layout.rows().iter().map(|row| (row.start(), row.end())));
            (layout.cursor_row(), layout.cursor_col())
        };
        let empty = self.editor.is_empty() && prompt == 0;
        // A padding row above the text, then the text rows.
        self.paint_box_row(y, width, &pen);
        if prompt > 0 {
            self.paint_box_row(y + 1, width, &pen);
            let text = self.editor.search_prompt().unwrap_or_default();
            if !text.is_empty() {
                self.screen.put_str(BAR + PAD, y + 1, text, pen.dim);
            }
        }
        for offset in prompt..shown {
            let at = y + 1 + offset;
            self.paint_box_row(at, width, &pen);
            if empty {
                if offset == prompt {
                    let placeholder = placeholder(self.running);
                    self.screen.put_str(BAR + PAD, at, placeholder, pen.dim);
                }
                continue;
            }
            if let Some(&(start, end)) = rows.get(offset as usize) {
                self.paint_editor_row(at, width, start, end, &pen);
            }
        }
        // A padding row below the text, the hint row of a held chip, then the
        // footer.
        let below = y + 1 + shown;
        self.paint_box_row(below, width, &pen);
        let footer = if hint {
            self.paint_box_row(below + 1, width, &pen);
            self.screen
                .put_str(BAR + PAD, below + 1, HINT_PASTE, pen.dim);
            below + 2
        } else {
            below + 1
        };
        self.paint_box_row(footer, width, &pen);
        self.paint_footer(footer, width, &pen);
        // The empty row between the box and the status line. The frame's fill
        // pass promotes the blank cells of this row too, so it keeps a style
        // that has no fill of its own.
        blank_row(&mut self.screen, footer + GAP, 0, width, spacer);
        // The frame carries one cursor, the terminal's own, on the edit point.
        if self.approval.is_some() {
            self.screen.clear_cursor();
        } else {
            // An open chip's gutter stands in front of its own text, so the
            // cursor on one moves with it.
            let gutter = u16::from(gutter_at(&self.editor, self.editor.cursor())) * GUTTER_WIDTH;
            let column = BAR + PAD + cursor_col + gutter;
            let row = y + 1 + (prompt + cursor_row).min(shown - 1);
            self.screen.set_cursor(column, row);
        }
        self.rows = rows;
        self.editor_top = y;
        given
    }

    /// Draws the open sheet in place of the box, on the rows the box would have
    /// drawn, and answers the rows it took. The frame's cursor goes where the
    /// sheet's filter row says.
    fn paint_sheet(&mut self, y: u16, height: u16, width: u16) -> u16 {
        if height == 0 {
            return 0;
        }
        let inks = self.sheet_inks();
        let Some(open) = self.sheet.as_ref() else {
            return 0;
        };
        let (column, row) = draw_sheet(&mut self.screen, &open.sheet, y, height, width, &inks);
        self.screen.set_cursor(column, row);
        // No chip of the box behind the sheet takes a click.
        self.rows.clear();
        self.editor_top = y;
        height
    }

    /// The styles the sheet draws with: the box's own roles over the fill of
    /// each kind of row, so a name, its gap and its detail all keep the fill.
    fn sheet_inks(&mut self) -> SheetInks {
        let theme = self.look.theme;
        let fill = theme.fill(Role::InputFill);
        let picked = theme.fill(Role::CardAccent);
        let query = theme.fill(Role::CodeFill);
        SheetInks {
            plain: sheet_row(&mut self.screen, theme, fill),
            picked: sheet_row(&mut self.screen, theme, picked),
            query: sheet_row(&mut self.screen, theme, query),
        }
    }

    /// The styles the list above the box draws with: the list's own fill and
    /// the picked row's, each with the roles over it.
    fn list_inks(&mut self) -> ListInks {
        let theme = self.look.theme;
        let fill = theme.fill(Role::SurfaceFill);
        let picked = theme.fill(Role::CardAccent);
        ListInks {
            plain: list_row(&mut self.screen, theme, fill),
            picked: list_row(&mut self.screen, theme, picked),
        }
    }

    /// Opens the folded paste chip at the clicked cell, and answers whether the
    /// click landed on one. A click on a chip selects it and opens it, the same
    /// as Tab on it.
    pub(crate) fn click_paste(&mut self, at: crate::mouse::Cell) -> bool {
        let top = self.editor_top;
        // The row above the text is the box's own padding.
        if at.row <= top {
            return false;
        }
        let offset = usize::from(at.row - (top + 1));
        let Some(&(start, end)) = self.rows.get(offset) else {
            return false;
        };
        let width = self.text_width();
        if at.column < BAR + PAD || at.column >= width {
            return false;
        }
        let column = at.column - (BAR + PAD);
        let mut hit = None;
        for (index, chip) in self.editor.pastes().iter().enumerate() {
            if chip.is_open() {
                continue;
            }
            let mark = chip.range().0;
            if mark < start || mark >= end {
                continue;
            }
            let (head, tail) = self.editor.slice(start, mark);
            let mut x = (UnicodeWidthStr::width(head) + UnicodeWidthStr::width(tail)) as u16;
            // An open chip before it on this row draws its gutter first.
            let gutter = self.editor.pastes().iter().any(|other| {
                other.is_open()
                    && other.range().0 < end
                    && start < other.range().1
                    && other.range().0 < mark
            });
            if gutter {
                x += GUTTER_WIDTH;
            }
            let stop = (x + chip.width()).min(width);
            if x < width && column >= x && column < stop {
                hit = Some(index);
                break;
            }
        }
        let Some(index) = hit else {
            return false;
        };
        let mark = self.editor.pastes()[index].range().0;
        while self.editor.cursor() > mark {
            self.editor.left();
        }
        while self.editor.cursor() < mark {
            self.editor.right();
        }
        self.editor.toggle_paste();
        self.editor_dirty = true;
        self.touch();
        true
    }

    /// Draws one row of the editor's text: the text itself, each folded chip in
    /// its place, and the `│` gutter an open chip puts in front of its lines. A
    /// box with no chip draws its whole row in one pass, as it did before the
    /// chips existed.
    fn paint_editor_row(&mut self, y: u16, width: u16, start: usize, end: usize, pen: &Pen) {
        if self.editor.pastes().is_empty() {
            let (head, tail) = self.editor.slice(start, end);
            let x = self.screen.put_str(BAR + PAD, y, head, pen.text);
            self.screen.put_str(x, y, tail, pen.text);
            return;
        }
        let selected = self.editor.selected_paste();
        // The open chip whose lines this row draws, if any.
        let open = self
            .editor
            .pastes()
            .iter()
            .position(|chip| chip.is_open() && chip.range().0 < end && start < chip.range().1);
        let style = if open.is_some() {
            // An open chip's rows stand a step darker than the box's own.
            blank_row(&mut self.screen, y, BAR, width, pen.open);
            pen.open
        } else {
            pen.text
        };
        let mut gutter = open.map(|index| self.editor.pastes()[index].range().0.max(start));
        let mut pos = start;
        let mut x = BAR + PAD;
        loop {
            let chip =
                self.editor.pastes().iter().position(|chip| {
                    !chip.is_open() && pos <= chip.range().0 && chip.range().0 < end
                });
            let event = match (
                chip.map(|index| self.editor.pastes()[index].range().0),
                gutter,
            ) {
                (Some(mark), Some(gutter)) => Some(mark.min(gutter)),
                (mark, gutter) => mark.or(gutter),
            };
            let stop = event.unwrap_or(end);
            let (head, tail) = self.editor.slice(pos, stop);
            x = self.screen.put_str(x, y, head, style);
            x = self.screen.put_str(x, y, tail, style);
            let Some(event) = event else {
                break;
            };
            if Some(event) == gutter {
                x = self.screen.put_str(x, y, GUTTER, pen.open_mark);
                gutter = None;
                pos = event;
                continue;
            }
            let Some(index) = chip else {
                break;
            };
            let held = if Some(index) == selected {
                pen.held
            } else {
                None
            };
            x = draw_chip(
                &mut self.screen,
                x,
                y,
                width,
                &self.editor.pastes()[index],
                held,
                pen,
            );
            pos = event + MARK_BYTES;
        }
    }

    /// Draws the whole width of one row of the box: the accent bar and the
    /// input fill behind everything else.
    fn paint_box_row(&mut self, y: u16, width: u16, pen: &Pen) {
        self.screen.put_str(0, y, BAR_GLYPH, pen.bar);
        blank_row(&mut self.screen, y, BAR, width, pen.text);
    }

    /// Draws the footer of the box at row `y`: the receiver and the model on the
    /// left, the keys of the state the app is in on the right.
    fn paint_footer(&mut self, y: u16, width: u16, pen: &Pen) {
        let mut x = self.screen.put_str(BAR, y, RECEIVER, pen.accent);
        let (model, settings) = self.footer_text();
        // The panel names the model as the settings name it, `provider/id`: the
        // footer takes the id in bold and the provider with the rest, in dim.
        let (name, provider) = match model.as_str().rsplit_once('/') {
            Some((provider, name)) => (name, Some(provider)),
            None => (model.as_str(), None),
        };
        if !name.is_empty() {
            x = self.screen.put_str(x, y, "  ", pen.text);
            x = self.screen.put_str(x, y, name, pen.bold);
        }
        if provider.is_some() || !settings.is_empty() {
            x = self.screen.put_str(x, y, "  ", pen.text);
        }
        if let Some(provider) = provider {
            x = self.screen.put_str(x, y, provider, pen.dim);
        }
        if !settings.is_empty() {
            if provider.is_some() {
                x = self.screen.put_str(x, y, " · ", pen.dim);
            }
            x = self.screen.put_str(x, y, settings.as_str(), pen.dim);
        }
        let keys = if self.running { KEYS_BUSY } else { KEYS_IDLE };
        let mut at = width.saturating_sub(1).saturating_sub(keys_width(keys));
        if at <= x {
            return;
        }
        for (index, (key, word)) in keys.iter().enumerate() {
            if index > 0 {
                at += 2;
            }
            at = self.screen.put_str(at, y, key, pen.text);
            at = self.screen.put_str(at, y, " ", pen.text);
            at = self.screen.put_str(at, y, word, pen.dim);
        }
    }

    /// The model and the settings line the footer names, from the Context
    /// panel: the app keeps them there for the sidebar and nowhere else, and a
    /// panel hands its text over as a shared `Arc`, so reading them costs no
    /// copy and no allocation after the first frame.
    fn footer_text(&mut self) -> (Text, Text) {
        let width = self.panel_width();
        let mut lines = std::mem::take(&mut self.editor.panel_lines);
        lines.clear();
        for index in 0..self.sidebar.len() {
            let Some(panel) = self.sidebar.panel(index) else {
                break;
            };
            if panel.id() != CONTEXT {
                continue;
            }
            panel.draw(Size::new(width, 1), &mut lines);
            break;
        }
        let mut model = Text::Empty;
        let mut settings = Text::Empty;
        for line in &lines {
            let Component::Text { text, role } = line else {
                continue;
            };
            if text.is_empty() || text.as_str() == NONE {
                continue;
            }
            match role {
                Line::Strong if model.is_empty() => model = text.clone(),
                Line::Dim if settings.is_empty() => settings = text.clone(),
                _ => {}
            }
        }
        self.editor.panel_lines = lines;
        (model, settings)
    }

    /// The width the sidebar draws its panels at: a row that reads at the right
    /// edge of a panel is built for the width it drew at, so the footer reads
    /// the panel at that same width and the row costs no build.
    fn panel_width(&self) -> u16 {
        let edge = u16::from(self.look.theme.fill(theme::Role::PanelFill) == Color::Default);
        let rect = self.aside.rect();
        rect.map_or(0, |rect| rect.width.saturating_sub(edge))
    }

    /// The rows the list above the text box takes: the open list, or the
    /// which-key list while Ctrl+X waits for its key.
    pub(crate) fn list_height(&self) -> u16 {
        if let Some(list) = &self.list {
            return list.height();
        }
        if self.keys.leader_pending() {
            return self.leader_list.height();
        }
        0
    }

    /// Draws the list above the text box at `y` and answers the rows it took.
    pub(crate) fn paint_list(&mut self, y: u16) -> u16 {
        let height = self.list_height();
        if height == 0 {
            return 0;
        }
        let width = self.text_width();
        let inks = self.list_inks();
        let waiting = self.files_pending()
            && matches!(self.list.as_ref().map(List::source), Some(Source::Files(_)));
        if waiting {
            for row in y..y + height {
                blank_row(&mut self.screen, row, 0, width, inks.plain.fill);
            }
            self.screen
                .put_str_until(2, y + height - 1, "reading files…", inks.plain.dim, width);
            return height;
        }
        let list = if self.list.is_some() {
            self.list.as_ref()
        } else {
            Some(&self.leader_list)
        };
        let Some(list) = list else {
            return 0;
        };
        draw_list(&mut self.screen, list, y, width, &inks)
    }

    /// Puts the terminal cursor on the palette's query row, where the next
    /// typed char lands; the box keeps it every other frame.
    pub(crate) fn paint_list_cursor(&mut self, y: u16) {
        let Some(list) = self.list.as_ref().filter(|list| list.is_palette()) else {
            return;
        };
        let width = self.text_width().max(1);
        let column = 2 + UnicodeWidthStr::width(list.query()) as u16;
        self.screen
            .set_cursor(column.min(width - 1), y.saturating_add(1));
    }

    /// The styles of the box for this frame: each role's own style with the
    /// input fill's background under it, interned once so a cell costs a lookup
    /// and never a style built beside it. The chip's styles come after, over the
    /// code fill and the fill of an open chip, and the held chip's three are
    /// interned only while one is held.
    fn box_pen(&mut self) -> Pen {
        let theme = self.look.theme;
        let fill = theme.fill(Role::InputFill);
        let code = theme.fill(Role::CodeFill);
        let open = open_fill(theme);
        let held = self.holds_folded_paste();
        let bar = filled(&mut self.screen, theme, fill, Role::Accent, Emphasis::Plain);
        let text = filled(&mut self.screen, theme, fill, Role::Text, Emphasis::Plain);
        let dim = filled(&mut self.screen, theme, fill, Role::Dim, Emphasis::Plain);
        let accent = filled(&mut self.screen, theme, fill, Role::Accent, Emphasis::Bold);
        let bold = filled(&mut self.screen, theme, fill, Role::Text, Emphasis::Bold);
        let pasted_mark = filled(&mut self.screen, theme, code, Role::Accent, Emphasis::Plain);
        let pasted = filled(&mut self.screen, theme, code, Role::Text, Emphasis::Plain);
        let pasted_count = filled(&mut self.screen, theme, code, Role::Dim, Emphasis::Plain);
        let open_mark = filled(&mut self.screen, theme, open, Role::Accent, Emphasis::Plain);
        let open_text = filled(&mut self.screen, theme, open, Role::Text, Emphasis::Plain);
        let held = held.then(|| Held {
            mark: underlined(&mut self.screen, theme, code, Role::Accent, Emphasis::Plain),
            line: underlined(&mut self.screen, theme, code, Role::Text, Emphasis::Plain),
            count: underlined(&mut self.screen, theme, code, Role::Dim, Emphasis::Plain),
        });
        Pen {
            bar,
            text,
            dim,
            accent,
            bold,
            pasted,
            pasted_mark,
            pasted_count,
            open: open_text,
            open_mark,
            held,
        }
    }
}

/// True when the chip at the edit point is open, so the cursor on it moves with
/// the gutter in front of its text.
fn gutter_at(editor: &crate::editor::Editor, cursor: usize) -> bool {
    editor
        .pastes()
        .iter()
        .any(|chip| chip.is_open() && chip.range().0 <= cursor && cursor <= chip.range().1)
}

/// Draws a folded chip at column `x` of row `y`: the `≡` in the accent, the
/// first line, then the count of the lines it holds back in dim, all on the code
/// fill the pen's styles carry. It stops at `width`, so a chip wider than the row
/// is cut to fit, and it copies nothing: the line is the range of the chip's one
/// buffer that was measured at paste.
fn draw_chip(
    screen: &mut Screen,
    x: u16,
    y: u16,
    width: u16,
    chip: &Paste,
    held: Option<Held>,
    pen: &Pen,
) -> u16 {
    let (mark, line, count) = match held {
        Some(held) => (held.mark, held.line, held.count),
        None => (pen.pasted_mark, pen.pasted, pen.pasted_count),
    };
    let mut x = screen.put_str_until(x, y, CHIP, mark, width);
    x = screen.put_str_until(x, y, " ", line, width);
    x = screen.put_str_until(x, y, chip.first_line(), line, width);
    x = screen.put_str_until(x, y, " ", line, width);
    x = screen.put_str_until(x, y, CHIP_MORE, count, width);
    let mut digits = [0u8; 20];
    let count_text = decimal(&mut digits, chip.other());
    screen.put_str_until(x, y, count_text, count, width)
}

/// Writes `n` in decimal at the end of `out` and answers the text: twenty digits
/// hold every `usize`, so counting the lines a chip holds back costs no heap.
fn decimal(out: &mut [u8; 20], mut n: usize) -> &str {
    let mut index = out.len();
    loop {
        index -= 1;
        out[index] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&out[index..]).expect("digits are UTF-8")
}

/// The fill a chip shows while it is open: a step darker than the box's own, the
/// mockup's mix of the code fill half into the input fill. A theme with no fills,
/// or one the depth cannot mix, keeps the code fill alone.
fn open_fill(theme: Theme) -> Color {
    match (theme.fill(Role::InputFill), theme.fill(Role::CodeFill)) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            Color::Rgb(mid(ar, br), mid(ag, bg), mid(ab, bb))
        }
        _ => theme.fill(Role::CodeFill),
    }
}

/// The middle of two channels, rounded up as a mix of half does.
fn mid(a: u8, b: u8) -> u8 {
    (u16::from(a) + u16::from(b)).div_ceil(2) as u8
}

/// The placeholder of the box: what a key does with the text, by whether a turn
/// runs.
fn placeholder(running: bool) -> &'static str {
    if running {
        PLACEHOLDER_BUSY
    } else {
        PLACEHOLDER
    }
}

/// The columns the keys of the footer take, two spaces between the pairs.
fn keys_width(keys: &[(&str, &str)]) -> u16 {
    let pairs = keys.len().saturating_sub(1) * 2;
    let text: usize = keys
        .iter()
        .map(|(key, word)| key.len() + word.len() + 1)
        .sum();
    (text + pairs) as u16
}

/// The styles one row of a sheet writes with, over `fill`: every role a row
/// draws, so no cell of the row falls back to the terminal's background.
fn sheet_row(screen: &mut Screen, theme: Theme, fill: Color) -> sheet::RowInks {
    sheet::RowInks {
        fill: screen.intern(Style {
            bg: fill,
            ..Style::default()
        }),
        bar: filled(screen, theme, fill, Role::Accent, Emphasis::Plain),
        text: filled(screen, theme, fill, Role::Text, Emphasis::Plain),
        bold: filled(screen, theme, fill, Role::Text, Emphasis::Bold),
        dim: filled(screen, theme, fill, Role::Dim, Emphasis::Plain),
        accent: filled(screen, theme, fill, Role::Accent, Emphasis::Plain),
        mark: filled(screen, theme, fill, Role::Accent, Emphasis::Bold),
        warn: filled(screen, theme, fill, Role::Warn, Emphasis::Plain),
        line: filled(screen, theme, fill, Role::Line, Emphasis::Plain),
    }
}

/// The styles one row of the list writes with, over `fill`.
fn list_row(screen: &mut Screen, theme: Theme, fill: Color) -> list::RowInks {
    list::RowInks {
        fill: screen.intern(Style {
            bg: fill,
            ..Style::default()
        }),
        text: filled(screen, theme, fill, Role::Text, Emphasis::Plain),
        bold: filled(screen, theme, fill, Role::Text, Emphasis::Bold),
        dim: filled(screen, theme, fill, Role::Dim, Emphasis::Plain),
        mark: filled(screen, theme, fill, Role::Accent, Emphasis::Bold),
    }
}

/// A role's style with `fill`'s background under it, interned into the screen's
/// table: a theme without a fill interns the role's own style, so the box holds
/// no extra style in terminal mode.
fn filled(screen: &mut Screen, theme: Theme, fill: Color, role: Role, emphasis: Emphasis) -> u32 {
    let mut style = theme.styled(role, emphasis);
    style.bg = fill;
    screen.intern(style)
}

/// A role's style over `fill` and underlined: the accent underline the mockup
/// puts under the chip the edit point holds.
fn underlined(
    screen: &mut Screen,
    theme: Theme,
    fill: Color,
    role: Role,
    emphasis: Emphasis,
) -> u32 {
    let mut style = theme.styled(role, emphasis);
    style.bg = fill;
    style.underline = true;
    screen.intern(style)
}

/// Blanks the row from column `from` to `width` in `style`, so every cell of the
/// box carries its band and no cell falls back to the terminal's background.
fn blank_row(screen: &mut Screen, y: u16, from: u16, width: u16, style: u32) {
    for x in from..width {
        screen.set(x, y, Cell::from_char(' ', style));
    }
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use crate::client::{Approval, Initialized};
    use crate::screen::Color;
    use crate::theme::{Depth, Emphasis, Rgb, Role};

    use crate::slot::Update;

    use super::super::super::{App, ApprovalBox};
    use super::{GAP, RECEIVER, TOP};

    /// The columns and the rows of the test terminal.
    const COLUMNS: u16 = 80;
    const ROWS: u16 = 24;

    /// The bar of the box: one quarter of a block, on every row of it.
    const BAR: char = '▎';

    /// The first line of the gate log the chip tests paste.
    const FIRST: &str = "error[E0308]: mismatched types";

    /// A paste of `lines` lines whose first line is [`FIRST`].
    fn log(lines: usize) -> String {
        let mut text = String::from(FIRST);
        for line in 1..lines {
            text.push_str("\n  line ");
            text.push_str(&line.to_string());
        }
        text
    }

    /// An app in full screen, its editor holding `text`.
    fn app_rows(text: &str, rows: u16) -> App {
        let mut app = App::new(&Initialized::default(), (COLUMNS, rows), true);
        app.editor.insert(text);
        app.editor_dirty = true;
        app
    }

    fn app(text: &str) -> App {
        app_rows(text, ROWS)
    }

    /// An app in full screen whose editor holds a bracketed paste of `lines`
    /// lines, folded into one chip.
    fn app_paste(lines: usize) -> App {
        let mut app = App::new(&Initialized::default(), (COLUMNS, ROWS), true);
        app.editor.paste(log(lines));
        app.editor_dirty = true;
        app
    }

    /// The text of one painted row, trailing blanks trimmed.
    fn row(app: &App, y: u16) -> String {
        let grid = app.screen.back();
        let mut text = String::new();
        for x in 0..grid.width() {
            match grid.cell(x, y).char() {
                Some(c) => text.push(c),
                None => text.push(' '),
            }
        }
        text.truncate(text.trim_end_matches(' ').len());
        text
    }

    /// The rows of the box and of the empty row under it: the rows the frame's
    /// fill pass promotes, which is what the theme tests call the box.
    fn box_rows(app: &App) -> std::ops::Range<u16> {
        let last = app.screen.rows().saturating_sub(1);
        last.saturating_sub(app.editor_rows) + TOP..last
    }

    /// The rows of the box itself, the empty row under it left out.
    fn box_only(app: &App) -> std::ops::Range<u16> {
        let rows = box_rows(app);
        rows.start..rows.end - GAP
    }

    /// The row of the footer: the last row of the box, the one with the
    /// receiver on it.
    fn footer(app: &App) -> u16 {
        box_only(app)
            .rev()
            .find(|y| row(app, *y).contains(RECEIVER))
            .expect("a footer row")
    }

    /// One frame drawn the way the run draws it, and the bytes it wrote.
    fn frame(app: &mut App) -> String {
        let mut bytes = Vec::new();
        app.paint();
        app.paint_input_fill();
        app.screen.diff(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Whether a row of the box holds the bar and nothing else.
    fn bare(app: &App, y: u16) -> bool {
        row(app, y)
            .strip_prefix(BAR)
            .is_some_and(|rest| rest.trim().is_empty())
    }

    /// The row the drawn screen shows `needle` on, the last one if several do.
    fn at(app: &App, needle: &str) -> u16 {
        (0..app.screen.rows())
            .rev()
            .find(|y| row(app, *y).contains(needle))
            .unwrap_or_else(|| panic!("no row shows {needle:?}"))
    }

    /// The bar and the blanks of every row of the box, and no reverse cell.
    fn assert_no_cursor_cell(app: &App) {
        for y in box_rows(app) {
            for x in 0..app.text_width() {
                let cell = app.screen.back().cell(x, y);
                assert!(
                    !app.screen.style_of(cell.style()).reverse,
                    "the cell at ({x}, {y}) is reversed: the frame's cursor is the \
                     terminal's own"
                );
                assert!(
                    cell.char().is_some() || cell.is_continuation(),
                    "the cell at ({x}, {y}) holds no glyph"
                );
            }
        }
    }

    #[test]
    fn the_text_box_frame_ends_with_the_cursor_at_the_edit_point() {
        let mut app = app("hi there");
        let bytes = frame(&mut app);
        assert!(
            bytes.starts_with("\x1b[?2026h\x1b[?25l"),
            "the frame hides the cursor first, inside its sync block: {bytes:?}"
        );
        let y = at(&app, "hi there");
        // The edit point is after the text, past the bar and the left padding.
        let place = format!("\x1b[{};{}H\x1b[?25h\x1b[?2026l", y + 1, 12);
        assert!(
            bytes.ends_with(&place),
            "the frame ends by placing and showing the cursor at ({}, {}): {bytes:?}",
            11,
            y
        );
        assert_no_cursor_cell(&app);
    }

    #[test]
    fn the_text_box_footer_names_the_model_in_bold_and_its_provider_dim() {
        let mut app = app("");
        app.panels_update(&Update::Model {
            name: "deepseek/deepseek-flash",
            window: 128_000,
        });
        let _ = frame(&mut app);
        let y = footer(&app);
        let text = row(&app, y);
        assert!(
            text.contains("reagent  deepseek-flash  deepseek"),
            "the model and its provider: {text:?}"
        );
        // The model reads bold, the provider and the settings dim.
        let at = |needle: &str| {
            let x = text.find(needle).expect("the footer holds it") as u16;
            app.screen.style_of(app.screen.back().cell(x, y).style())
        };
        assert!(at("deepseek-flash").bold, "the model in bold: {text:?}");
        assert!(at(" deepseek ").dim, "the provider in dim: {text:?}");
    }

    #[test]
    fn the_text_box_footer_shows_the_busy_keys_while_a_turn_runs() {
        let mut app = app("");
        app.running = true;
        let _ = frame(&mut app);
        let busy = row(&app, footer(&app));
        assert!(
            busy.contains("enter steer  alt+enter queue"),
            "the busy keys: {busy:?}"
        );
        assert!(busy.contains("reagent"), "the receiver: {busy:?}");

        app.running = false;
        app.status_dirty = true;
        let _ = frame(&mut app);
        let idle = row(&app, footer(&app));
        assert!(
            idle.contains("enter send  alt+enter queue"),
            "the idle keys: {idle:?}"
        );
        assert!(!idle.contains("steer"), "no busy key when idle: {idle:?}");
    }

    #[test]
    fn the_text_box_placeholder_names_steering_while_busy_and_asking_when_idle() {
        let mut app = app("");
        app.running = true;
        let _ = frame(&mut app);
        assert_eq!(
            row(&app, at(&app, "Steer the run")),
            "▎  Steer the run · alt+enter queues for the next turn",
            "the busy placeholder sits past the bar and the padding"
        );

        app.running = false;
        app.status_dirty = true;
        let _ = frame(&mut app);
        assert!(
            row(&app, at(&app, "Ask anything")).contains("Ask anything · / commands · @ files"),
            "the idle placeholder"
        );
    }

    #[test]
    fn the_text_box_grows_to_a_third_of_a_thirty_row_screen() {
        let mut app = app_rows("", 30);
        for line in 0..20 {
            app.editor.insert(&format!("line {line}\n"));
        }
        app.editor_dirty = true;
        let bytes = frame(&mut app);
        // A third of 30 rows is 10: seven text rows, a padding row above and
        // below them and the footer, then the empty rows above the box and
        // under it.
        assert_eq!(
            app.editor_rows, 12,
            "the box stops at a third of the screen"
        );

        let rows: Vec<String> = (0..30).map(|y| row(&app, y)).collect();
        assert!(
            rows[19].starts_with("▎  line 14"),
            "the text scrolled to keep the edit point in the window: {:?}",
            &rows[17..26]
        );
        assert!(
            bytes.ends_with("\x1b[26;4H\x1b[?25h\x1b[?2026l"),
            "the cursor sits on the last text row of the window: {bytes:?}"
        );
    }

    #[test]
    fn the_text_box_leaves_one_empty_row_between_it_and_the_status() {
        let mut app = app("a line");
        let _ = frame(&mut app);
        let last = app.screen.rows() - 1;
        assert!(
            row(&app, last).contains("idle"),
            "the status line is the last row"
        );
        assert_eq!(
            row(&app, last - 1).trim(),
            "",
            "one empty row between the box and the status line"
        );
        let footer = footer(&app);
        assert_eq!(footer, last - 2, "the footer is the last row of the box");
        assert!(bare(&app, footer - 1), "the padding row below the text");
        assert_eq!(
            row(&app, footer - 2),
            "▎  a line",
            "the text row, with the bar and the padding"
        );
        assert!(bare(&app, footer - 3), "the padding row above the text");
    }

    #[test]
    fn the_text_box_leaves_one_empty_row_between_a_reply_and_it() {
        let mut app = app("a line");
        // A reply still streaming draws in the live area, right above the box.
        app.text("the last reply row");
        let _ = frame(&mut app);
        let footer = footer(&app);
        let top = footer - 3;
        let screen: Vec<String> = (0..ROWS).map(|y| row(&app, y)).collect();
        assert!(bare(&app, top), "the padding row above the text");
        assert_eq!(row(&app, top - 1).trim(), "", "one empty row above the box");
        assert!(
            row(&app, top - 2).contains("the last reply row"),
            "the reply's last row sits over the empty row:\n{}",
            screen.join("\n")
        );
    }

    #[test]
    fn the_text_box_in_terminal_mode_draws_no_fill_and_keeps_the_bar() {
        let mut app = app("a line");
        // The mode is the terminal's until it answers the background query.
        let _ = frame(&mut app);
        for y in box_only(&app) {
            let bar = app.screen.back().cell(0, y);
            assert_eq!(
                bar.char(),
                Some(BAR),
                "every row of the box keeps the bar, row {y}"
            );
            assert_eq!(
                bar.style(),
                app.style(Role::Accent, Emphasis::Plain),
                "the bar draws in the accent role"
            );
        }
        for y in box_rows(&app) {
            for x in 0..app.text_width() {
                let cell = app.screen.back().cell(x, y);
                assert_eq!(
                    app.screen.style_of(cell.style()).bg,
                    Color::Default,
                    "the cell at ({x}, {y}) has a fill in terminal mode"
                );
            }
        }
    }

    #[test]
    fn the_text_box_hides_the_cursor_while_an_approval_waits() {
        let mut app = app("a line");
        app.approval = Some(ApprovalBox::new(
            Approval {
                id: 1,
                tool: "bash".into(),
                call: "rm -rf target".into(),
                rule: "outside the project".into(),
                save: true,
            },
            COLUMNS,
        ));
        let bytes = frame(&mut app);
        assert!(
            bytes.contains("\x1b[?25l"),
            "the frame hides the cursor: {bytes:?}"
        );
        assert!(
            !bytes.contains("\x1b[?25h"),
            "the cursor stays hidden while the card waits: {bytes:?}"
        );
    }

    /// The column the text `needle` starts at on row `y` of the screen.
    fn column(app: &App, y: u16, needle: &str) -> u16 {
        let text = row(app, y);
        let at = text.find(needle).expect("the row holds it");
        UnicodeWidthStr::width(&text[..at]) as u16
    }

    /// The style the text `needle` draws with on row `y` of the screen.
    fn style_at(app: &App, y: u16, needle: &str) -> crate::screen::Style {
        let x = column(app, y, needle);
        app.screen.style_of(app.screen.back().cell(x, y).style())
    }

    /// The style a role draws in, as the app interned it.
    fn role_style(app: &App, role: Role) -> crate::screen::Style {
        app.screen.style_of(app.style(role, Emphasis::Plain))
    }

    /// An app in rich mode, the way the terminal's background answer leaves it.
    fn rich(mut app: App) -> App {
        app.look.color = true;
        app.look.depth = Depth::TrueColor;
        app.answered(Rgb::new(0x14, 0x14, 0x14));
        app
    }

    /// The cell the frame ends by placing the terminal's own cursor on: the
    /// column and the row it names, one-based as the sequence does.
    fn placed(bytes: &str) -> (u16, u16) {
        let shown = bytes
            .rfind("\x1b[?25h")
            .expect("the frame shows the cursor again");
        let start = bytes[..shown]
            .rfind("\x1b[")
            .expect("a place sequence before it");
        let (row, column) = bytes[start + 2..shown]
            .strip_suffix('H')
            .expect("the place sequence ends with H")
            .split_once(';')
            .expect("a row and a column");
        (
            column.parse().expect("a column"),
            row.parse().expect("a row"),
        )
    }

    /// The column just past the last cell `needle` draws in on `y`, one-based.
    fn after(app: &App, y: u16, needle: &str) -> u16 {
        let text = row(app, y);
        let end = text.find(needle).expect("the row draws it") + needle.len();
        UnicodeWidthStr::width(&text[..end]) as u16 + 1
    }

    #[test]
    fn the_cursor_stands_after_a_folded_paste_chip() {
        let mut app = app_paste(142);
        let bytes = frame(&mut app);
        let y = at(&app, FIRST);
        assert_eq!(
            placed(&bytes),
            (after(&app, y, "… +141"), y + 1),
            "the cursor stands after the chip, not inside it: {:?}",
            row(&app, y)
        );

        // Typing behind the chip keeps the cursor after the last typed character
        // instead of one cell into the chip.
        app.editor.insert(" why");
        app.editor_dirty = true;
        let bytes = frame(&mut app);
        let y = at(&app, "why");
        assert_eq!(
            placed(&bytes),
            (after(&app, y, "why"), y + 1),
            "the cursor stands after {:?}: {:?}",
            " why",
            row(&app, y)
        );
    }

    #[test]
    fn a_chip_that_does_not_fit_the_row_wraps_where_it_is_drawn() {
        // Sixty cells of text then a chip of 37: the text row has 77 cells, so
        // the chip starts the row below the one it was pasted on.
        let mut app = app(&"x".repeat(60));
        app.editor.paste(log(142));
        app.editor_dirty = true;
        let bytes = frame(&mut app);
        let x = "x".repeat(60);
        let y = at(&app, &x);
        assert_eq!(
            row(&app, y),
            format!("▎  {x}"),
            "the text row holds the sixty x's and none of the chip"
        );
        let below = y + 1;
        assert!(
            row(&app, below).starts_with(&format!("▎  ≡ {FIRST}")),
            "the chip starts the row below, where the layout wraps it: {:?}",
            row(&app, below)
        );
        assert_eq!(
            placed(&bytes),
            (after(&app, below, "… +141"), below + 1),
            "the cursor follows the chip on the row it wrapped to"
        );
    }

    #[test]
    fn a_142_line_paste_draws_as_one_chip() {
        let mut app = app_paste(142);
        let _ = frame(&mut app);
        let y = at(&app, FIRST);
        assert_eq!(
            row(&app, y),
            format!("▎  ≡ {FIRST} … +141"),
            "the chip draws inline at the paste point"
        );
        assert_eq!(
            style_at(&app, y, "≡").fg,
            role_style(&app, Role::Accent).fg,
            "the mark of the chip in the accent"
        );
        assert_eq!(
            style_at(&app, y, "… +141").fg,
            role_style(&app, Role::Dim).fg,
            "the count of the lines behind it in dim"
        );
        assert_eq!(
            style_at(&app, y, FIRST).fg,
            role_style(&app, Role::Text).fg,
            "the first line in the text's own color"
        );
    }

    #[test]
    fn a_folded_paste_draws_on_the_code_fill() {
        let mut app = rich(app_paste(142));
        let _ = frame(&mut app);
        let y = at(&app, FIRST);
        let code = app.look.theme.fill(Role::CodeFill);
        assert_ne!(
            code,
            app.look.theme.fill(Role::InputFill),
            "rich mode mixes the code fill away from the box's"
        );
        let chip = "≡ error[E0308]: mismatched types … +141";
        for x in 3..3 + UnicodeWidthStr::width(chip) as u16 {
            let cell = app.screen.back().cell(x, y);
            assert_eq!(
                app.screen.style_of(cell.style()).bg,
                code,
                "the cell at ({x}, {y}) is on the chip's fill"
            );
        }
    }

    #[test]
    fn the_paste_at_the_edit_point_draws_underlined_with_its_hint() {
        let mut app = rich(app_paste(142));
        let _ = frame(&mut app);
        let y = at(&app, FIRST);
        let chip = style_at(&app, y, FIRST);
        assert!(
            chip.underline,
            "the chip the edit point holds is underlined"
        );
        assert_eq!(
            chip.bg,
            app.look.theme.fill(Role::CodeFill),
            "still on the code fill"
        );
        let hint = at(&app, "tab or a click opens the paste to edit");
        assert!(
            hint > y,
            "the hint row sits under the text: chip {y}, hint {hint}"
        );
        assert!(
            style_at(&app, hint, "tab or a click").dim,
            "the hint reads dim"
        );

        // An edit point away from the chip leaves it plain and drops the hint.
        app.editor.line_start();
        app.editor.insert("here is the log ");
        app.editor.line_start();
        app.editor_dirty = true;
        let _ = frame(&mut app);
        let y = at(&app, FIRST);
        assert!(
            !style_at(&app, y, FIRST).underline,
            "no underline while the edit point is away from the chip"
        );
        assert!(
            !(0..ROWS).any(|y| row(&app, y).contains("backspace removes it")),
            "and no hint row"
        );
    }

    #[test]
    fn tab_opens_the_paste_in_place_behind_a_gutter() {
        let mut app = app_paste(4);
        assert!(app.editor.toggle_paste(), "tab opens the chip");
        app.editor_dirty = true;
        let bytes = frame(&mut app);
        let y = at(&app, FIRST);
        assert_eq!(
            row(&app, y),
            format!("▎  │ {FIRST}"),
            "the opened chip's first line behind the gutter"
        );
        assert_eq!(
            row(&app, y + 1),
            "▎  │   line 1",
            "and every row of the paste draws the same gutter"
        );
        assert!(
            !(0..ROWS).any(|y| row(&app, y).contains("backspace removes it")),
            "no hint while it is open"
        );
        // The cursor stands after the gutter, at the head of the pasted bytes.
        assert!(
            bytes.ends_with(&format!("\x1b[{};6H\x1b[?25h\x1b[?2026l", y + 1)),
            "the cursor follows the gutter: {bytes:?}"
        );
    }

    #[test]
    fn an_open_paste_draws_a_step_darker_than_the_box() {
        let mut app = rich(app_paste(4));
        assert!(app.editor.toggle_paste());
        app.editor_dirty = true;
        let _ = frame(&mut app);
        let y = at(&app, FIRST);
        let fill = style_at(&app, y, FIRST).bg;
        let input = app.look.theme.fill(Role::InputFill);
        let code = app.look.theme.fill(Role::CodeFill);
        assert_ne!(fill, input, "the opened chip is not on the box's fill");
        assert_ne!(fill, code, "and not on the folded chip's fill either");
        assert_eq!(
            style_at(&app, y, "│").fg,
            role_style(&app, Role::Accent).fg,
            "the gutter in the accent"
        );
    }

    #[test]
    fn a_paste_wider_than_the_row_is_cut_to_fit() {
        let long = format!("error: {}", "wide ".repeat(40));
        let mut app = App::new(&Initialized::default(), (COLUMNS, ROWS), true);
        app.editor.paste(format!("{long}\n1\n2\n3"));
        app.editor_dirty = true;
        let _ = frame(&mut app);
        let y = at(&app, "≡ error:");
        assert_eq!(
            UnicodeWidthStr::width(row(&app, y).as_str()),
            COLUMNS as usize,
            "the chip fills the row and stops there"
        );
        assert!(
            !row(&app, y).contains("+3"),
            "the count is cut with the rest: {}",
            row(&app, y)
        );
    }

    #[test]
    fn the_folded_paste_frame_holds_its_buffers() {
        let mut app = app_paste(142);
        let _ = frame(&mut app);
        let grid = (
            app.screen.back().cells_capacity(),
            app.screen.back().dirty_capacity(),
        );
        let styles = app.screen.style_count();
        let graphemes = app.screen.grapheme_count();
        let (text, chips) = (app.editor.capacity(), app.editor.chip_capacity());
        let rows = app.rows.capacity();
        for _ in 0..8 {
            let _ = frame(&mut app);
        }
        assert_eq!(
            (
                app.screen.back().cells_capacity(),
                app.screen.back().dirty_capacity()
            ),
            grid,
            "no cell and no dirty row is added"
        );
        assert_eq!(app.screen.style_count(), styles, "no style is added");
        assert_eq!(
            app.screen.grapheme_count(),
            graphemes,
            "no grapheme is added"
        );
        assert_eq!(app.editor.capacity(), text, "the text keeps its room");
        assert_eq!(app.editor.chip_capacity(), chips, "the chips keep theirs");
        assert_eq!(app.rows.capacity(), rows, "the row list keeps its room");
    }
}
