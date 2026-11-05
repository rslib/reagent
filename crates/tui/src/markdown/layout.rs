//! Lay a parsed block out for a width.

use std::fmt::Write as _;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    Block, Body, Cell, Kind, LLine, Link, Marker, Piece, Resume, Roles, Rows, Source, Table,
    TailMark,
};
use crate::highlight::{Lang, Span, State, tokenize};
use crate::screen::{Color, Style, StyleTable};

/// The columns between two table columns.
const ROW_GAP: usize = 2;

/// The columns a code block keeps before its text.
const CODE_INDENT: usize = 2;

#[cfg(test)]
thread_local! {
    /// The logical lines laid out since the last [`laid_reset`], so a test can
    /// check that a delta re-lays only the tail of the open block.
    static LAID: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The lines laid out since [`laid_reset`].
#[cfg(test)]
pub(super) fn laid() -> usize {
    LAID.with(std::cell::Cell::get)
}

/// Forget the lines laid out so far.
#[cfg(test)]
pub(super) fn laid_reset() {
    LAID.with(|laid| laid.set(0));
}

/// The text a layout reads.
pub(super) struct Src<'a> {
    pub text: &'a str,
    pub lit: &'a str,
    pub osc8: bool,
}

/// What the open block holds back until more text arrives.
#[derive(Clone, Copy, Debug)]
pub(super) struct Hold {
    /// The message text before this byte is shown; a trailing run of markup
    /// characters after it waits for the next delta.
    pub visible_end: usize,
    /// The last drawn line waits until the next line starts.
    pub hide_last: bool,
    /// The one line of a paragraph that a delimiter row may turn into a table.
    pub pending_table: bool,
    /// A closing fence the text has started but not finished.
    pub partial_fence: bool,
}

impl Default for Hold {
    fn default() -> Self {
        Hold {
            visible_end: usize::MAX,
            hide_last: false,
            pending_table: false,
            partial_fence: false,
        }
    }
}

/// One block being laid out for a width: the text it reads and the buffer it
/// writes the text it generates into.
pub(super) struct Lay<'a, 'b> {
    pub width: u16,
    pub src: &'a Src<'a>,
    pub styles: &'b mut StyleTable,
    pub roles: Roles,
    /// The interned styles of inline code.
    pub codes: &'b [u32],
    pub pad: &'b mut String,
    /// The tokenizer of the code blocks.
    pub highlight: &'b mut Highlighter,
    /// Draw one blank row above the block the next call lays out, once it draws
    /// a line of its own.
    pub gap: bool,
}

/// The tokenizer of the code blocks laid out for one message: the state a block
/// has carried to the line being laid out, and the span buffer it fills.
#[derive(Debug, Default)]
pub(super) struct Highlighter {
    /// The byte a block starts at, since a block's lines are laid out in order.
    block: usize,
    /// How many of the block's lines the state covers.
    line: usize,
    state: State,
    spans: Vec<Span>,
}

impl Highlighter {
    /// Carry the state to line `line` of the block at `block`, tokenizing the
    /// lines before it that this layout has not tokenized.
    fn at(&mut self, block: usize, line: usize, lines: &[LLine], src: &Src<'_>, lang: &Lang) {
        if self.block != block || self.line > line {
            self.block = block;
            self.line = 0;
            self.state = State::default();
        }
        while self.line < line {
            if let Some((_, text)) = lines.get(self.line).and_then(|line| code_text(line, src)) {
                tokenize(lang, text, &mut self.state, &mut self.spans);
            }
            self.line += 1;
        }
    }
}

impl Lay<'_, '_> {
    /// Append the lines of one block to `out`.
    pub(super) fn block(
        &mut self,
        block: &Block,
        out: &mut Rows,
        scratch: &mut Vec<Piece>,
        hold: Hold,
    ) {
        self.lines(block, 0, out, scratch, hold, None);
    }

    /// Append the lines of one block from `from` on, recording where its last
    /// line starts when `tail` is given, so a later delta can re-lay that line
    /// alone.
    pub(super) fn lines(
        &mut self,
        block: &Block,
        from: usize,
        out: &mut Rows,
        scratch: &mut Vec<Piece>,
        hold: Hold,
        mut tail: Option<&mut TailMark>,
    ) {
        let lines = match &block.body {
            Body::Flow(lines) => lines,
            // A table is laid out again from its first row, which is where the
            // mark stays for it: every column's width depends on every row.
            Body::Table(table) => {
                if let Some(mark) = tail.as_mut() {
                    **mark = TailMark {
                        line: 0,
                        resume: Resume {
                            rows: out.count(),
                            pad: self.pad.len(),
                            ..Resume::default()
                        },
                        ..TailMark::default()
                    };
                }
                return self.table(table, out, scratch);
            }
        };
        if self.is_code(block) {
            return self.code(block, from, out, scratch, hold, tail);
        }
        let count = lines.len();
        // Every line before `from` is drawn, so the mark names the line the
        // layout stops at: the last one, or the one a hold hides below it.
        if let Some(mark) = tail.as_mut() {
            **mark = TailMark {
                line: from.max(count.saturating_sub(1)),
                resume: Resume {
                    rows: out.count(),
                    pad: self.pad.len(),
                    ..Resume::default()
                },
                ..TailMark::default()
            };
        }
        for (i, line) in lines.iter().enumerate().skip(from) {
            if hold.pending_table && count == 1 {
                continue;
            }
            if hold.partial_fence && i + 1 == count {
                continue;
            }
            if self.gap {
                out.row();
                self.gap = false;
            }
            if i + 1 == count
                && let Some(mark) = tail.as_mut()
            {
                **mark = TailMark {
                    line: i,
                    resume: Resume {
                        rows: out.count(),
                        pad: self.pad.len(),
                        ..Resume::default()
                    },
                    ..TailMark::default()
                };
            }
            scratch.clear();
            self.pieces_of(line, &block.links, scratch);
            let mark = if i + 1 == count {
                tail.as_deref_mut()
            } else {
                None
            };
            self.draw(line, scratch, out, hold.visible_end, mark);
        }
    }

    /// Append what a block's tail mark covers and nothing before it: the rows
    /// and the pad the layout before it drew stay, and the last line is drawn
    /// again from the row the mark names.
    pub(super) fn tail(
        &mut self,
        block: &Block,
        mark: &mut TailMark,
        out: &mut Rows,
        scratch: &mut Vec<Piece>,
        hold: Hold,
    ) {
        self.pad.truncate(mark.resume.pad);
        out.truncate(mark.resume.rows);
        self.gap = false;
        let Body::Flow(lines) = &block.body else {
            // A table depends on every row, so it is laid out whole.
            return self.lines(block, 0, out, scratch, hold, Some(mark));
        };
        if self.is_code(block) {
            return self.code_tail(block, mark, out, scratch, hold);
        }
        let count = lines.len();
        if mark.line + 1 < count {
            // Lines arrived after the mark: finish the marked line from the row
            // the mark names, then lay out the lines that follow it, which have
            // no rows yet. Drawing the marked line from its start instead would
            // repeat the rows it already drew.
            let (index, resume) = (mark.line, mark.resume);
            if let Some(line) = lines.get(index) {
                scratch.clear();
                self.pieces_of(line, &block.links, scratch);
                self.draw_from(line, scratch, out, hold.visible_end, resume, None);
            }
            return self.lines(block, index + 1, out, scratch, hold, Some(mark));
        }
        if hold.pending_table && count == 1 {
            return;
        }
        if hold.partial_fence && mark.line + 1 == count {
            return;
        }
        let Some(line) = lines.get(mark.line) else {
            return;
        };
        scratch.clear();
        self.pieces_of(line, &block.links, scratch);
        let resume = mark.resume;
        self.draw_from(line, scratch, out, hold.visible_end, resume, Some(mark));
    }

    /// Whether a block draws as a code block: a fenced block, whose first line
    /// carries the fence's tag, or an indented one, whose lines are all code.
    fn is_code(&self, block: &Block) -> bool {
        let Body::Flow(lines) = &block.body else {
            return false;
        };
        let mut rest = lines.iter();
        match rest.next().map(|line| line.kind) {
            // A fenced block's tag line, then the code lines under it.
            Some(Kind::Text) if block.fence.is_some() => rest.all(|line| line.kind == Kind::Code),
            // An indented block: every line of it is code.
            Some(Kind::Code) => rest.all(|line| line.kind == Kind::Code),
            _ => false,
        }
    }

    /// Append the rows of a code block: the fence's tag above the code, the
    /// lines on the block's fill with two columns before their text, and a fill
    /// row below them.
    fn code(
        &mut self,
        block: &Block,
        from: usize,
        out: &mut Rows,
        scratch: &mut Vec<Piece>,
        hold: Hold,
        mut tail: Option<&mut TailMark>,
    ) {
        let Body::Flow(lines) = &block.body else {
            return;
        };
        let count = lines.len();
        let tag = lines.first().filter(|line| line.kind == Kind::Text);
        let lang = tag
            .and_then(|line| code_text(line, self.src))
            .and_then(|(_, text)| Lang::from_tag(text));
        if from == 0 {
            if self.gap {
                out.row();
                self.gap = false;
            }
            self.tag_row(tag.map(|line| line.pieces.as_slice()), out);
        }
        for (i, line) in lines.iter().enumerate().skip(from) {
            if i == 0 && tag.is_some() {
                continue;
            }
            let last = i + 1 == count;
            if last && let Some(mark) = tail.as_deref_mut() {
                let start = Resume {
                    rows: out.count(),
                    pad: self.pad.len(),
                    ..Resume::default()
                };
                *mark = TailMark {
                    line: i,
                    first: start,
                    resume: start,
                    ended: ended(self.src.text, i, count, line),
                };
            }
            if hold.partial_fence && last {
                continue;
            }
            // The empty line a block's last newline makes holds no row of its
            // own: the fill row below the code covers it.
            if last && line.pieces.is_empty() {
                continue;
            }
            self.code_pieces(block.start, i, lines, line, lang, scratch);
            let mark = if last { tail.as_deref_mut() } else { None };
            self.code_line(line, scratch, out, Resume::default(), mark);
        }
        self.fill_row(out);
    }

    /// Append what a code block's tail mark covers and nothing before it: the
    /// row the marked line stopped at, and the fill row below the code.
    fn code_tail(
        &mut self,
        block: &Block,
        mark: &mut TailMark,
        out: &mut Rows,
        scratch: &mut Vec<Piece>,
        hold: Hold,
    ) {
        let Body::Flow(lines) = &block.body else {
            return;
        };
        let count = lines.len();
        if hold.partial_fence && mark.line + 1 >= count {
            return self.fill_row(out);
        }
        let Some(line) = lines.get(mark.line) else {
            return;
        };
        // The only line of a code block that is not code is the fence's tag,
        // which the tag row draws: a mark still on it names the block's start,
        // so the block is laid out from there.
        if line.kind != Kind::Code {
            return self.code(block, 0, out, scratch, hold, Some(mark));
        }
        let tag = lines.first().filter(|line| line.kind == Kind::Text);
        let lang = tag
            .and_then(|line| code_text(line, self.src))
            .and_then(|(_, text)| Lang::from_tag(text));
        let now = ended(self.src.text, mark.line, count, line);
        // A line whose tokens changed while it was open is drawn again whole:
        // the plain rows it drew before are not what its tokens say.
        let resume = if now == mark.ended {
            mark.resume
        } else {
            mark.ended = now;
            self.pad.truncate(mark.first.pad);
            out.truncate(mark.first.rows);
            mark.first
        };
        self.code_pieces(block.start, mark.line, lines, line, lang, scratch);
        if mark.line + 1 < count {
            self.code_line(line, scratch, out, resume, None);
            return self.code(block, mark.line + 1, out, scratch, hold, Some(mark));
        }
        if line.pieces.is_empty() {
            return self.fill_row(out);
        }
        self.code_line(line, scratch, out, resume, Some(mark));
        self.fill_row(out);
    }

    /// The pieces of one line of a code block: its tokens in their roles once it
    /// has ended, its text in the code role until then.
    fn code_pieces(
        &mut self,
        block: usize,
        index: usize,
        lines: &[LLine],
        line: &LLine,
        lang: Option<Lang>,
        out: &mut Vec<Piece>,
    ) {
        out.clear();
        let Some((start, text)) = code_text(line, self.src) else {
            out.extend_from_slice(&line.pieces);
            return;
        };
        let Some(lang) = lang.filter(|_| ended(self.src.text, index, lines.len(), line)) else {
            out.extend_from_slice(&line.pieces);
            return;
        };
        let src = self.src;
        self.highlight.at(block, index, lines, src, &lang);
        let spans = {
            let highlighter = &mut self.highlight;
            tokenize(&lang, text, &mut highlighter.state, &mut highlighter.spans);
            highlighter.spans.len()
        };
        for i in 0..spans {
            let span = self.highlight.spans[i];
            let style = self.styles.intern(self.roles.token(span.token));
            out.push(Piece::new(
                Source::Text,
                start + span.start as usize,
                start + span.end as usize,
                style,
            ));
        }
    }

    /// Draw one line of a code block: its margin, its text on as many rows as it
    /// needs, and the block's fill to the end of every row.
    fn code_line(
        &mut self,
        line: &LLine,
        pieces: &[Piece],
        out: &mut Rows,
        resume: Resume,
        mut mark: Option<&mut TailMark>,
    ) {
        #[cfg(test)]
        LAID.with(|laid| laid.set(laid.get() + 1));
        let fill = self.styles.intern(self.roles.block);
        let (piece, off) = locate(pieces, resume.byte);
        let mut wrap = Wrap {
            pieces,
            text: self.src.text,
            lit: self.src.lit,
            visible_end: usize::MAX,
            piece,
            off,
            code: true,
        };
        let mut first = !resume.cont;
        let mut last: Option<Resume> = None;
        let mut drew = false;
        loop {
            // An empty line of code still holds a row of the block's fill.
            if drew && wrap.done(self.pad) {
                break;
            }
            let start = (
                out.count(),
                self.pad.len(),
                byte_of(pieces, wrap.piece, wrap.off),
            );
            let row = out.row();
            let used = self.code_prefix(line, row, fill);
            let avail = (self.width as usize).saturating_sub(used).max(1);
            let placed = wrap.fill(row, avail, self.pad);
            let room = (self.width as usize).saturating_sub(used + placed);
            if room > 0 && self.filled() {
                row.push(spaces(self.pad, room, fill));
            }
            last = Some(Resume {
                rows: start.0,
                pad: start.1,
                byte: start.2,
                cont: !first,
                ..Resume::default()
            });
            first = false;
            drew = true;
        }
        if let (Some(mark), Some(last)) = (mark.as_mut(), last) {
            mark.resume = last;
        }
    }

    /// The left margin of a code line: its quote bars and the columns the block
    /// keeps before the text. Answers the columns taken.
    fn code_prefix(&mut self, line: &LLine, row: &mut Vec<Piece>, fill: u32) -> usize {
        let quote = self.styles.intern(self.roles.quote);
        let mut used = 0;
        for _ in 0..line.quote {
            row.push(text_pad(self.pad, "▎ ", quote));
            used += 2;
        }
        let indent = line.indent as usize + CODE_INDENT;
        row.push(spaces(self.pad, indent, fill));
        used + indent
    }

    /// The row above a code block: the block's fill across the width, with the
    /// fence's tag dim one column from its right end.
    fn tag_row(&mut self, tag: Option<&[Piece]>, out: &mut Rows) {
        let width = self.width as usize;
        let filled = self.filled();
        let fill = self.styles.intern(self.roles.block);
        let label = self.styles.intern(self.roles.label);
        let tag_width = tag.map_or(0, |tag| self.pieces_width(tag).min(width.saturating_sub(1)));
        let lead = width.saturating_sub(tag_width + 1);
        let row = out.row();
        if lead > 0 {
            row.push(spaces(self.pad, lead, if filled { fill } else { 0 }));
        }
        if let Some(tag) = tag {
            self.tag_into(tag, tag_width, row, label);
        }
        let rest = width.saturating_sub(lead + tag_width);
        if filled && rest > 0 {
            row.push(spaces(self.pad, rest, fill));
        }
    }

    /// A row of the block's fill across the width.
    fn fill_row(&mut self, out: &mut Rows) {
        let row = out.row();
        if !self.filled() {
            return;
        }
        let fill = self.styles.intern(self.roles.block);
        row.push(spaces(self.pad, self.width as usize, fill));
    }

    /// The tag's pieces cut to `room` columns, in the block's label style.
    fn tag_into(&self, tag: &[Piece], room: usize, row: &mut Vec<Piece>, style: u32) {
        let mut used = 0;
        for piece in tag {
            if used >= room {
                break;
            }
            let text = self.text_of(piece);
            let columns = width(text);
            if used + columns <= room {
                row.push(Piece::new(
                    piece.source(),
                    piece.start(),
                    piece.end(),
                    style,
                ));
                used += columns;
                continue;
            }
            let fits = fit(text, room - used);
            if fits > 0 {
                row.push(Piece::new(
                    piece.source(),
                    piece.start(),
                    piece.start() + fits,
                    style,
                ));
            }
            break;
        }
    }

    /// The columns a run of pieces takes.
    fn pieces_width(&self, pieces: &[Piece]) -> usize {
        pieces.iter().map(|piece| width(self.text_of(piece))).sum()
    }

    /// Whether the block's fill has a color: terminal mode and `NO_COLOR` do not
    /// fill a block, and its padding rows stay empty.
    fn filled(&self) -> bool {
        self.roles.block.bg != Color::Default
    }

    /// The pieces of one line: the message spans, with a space of fill on each
    /// side of inline code and a link's destination after its label when OSC 8
    /// is off.
    fn pieces_of(&mut self, line: &LLine, links: &[Link], out: &mut Vec<Piece>) {
        for piece in &line.pieces {
            // A code line is drawn by the block's own layout, so it takes no
            // inline code fill here: a fence inside a list or a quote still
            // wraps at the column and keeps its own indent.
            let code = line.kind != Kind::Code && self.codes.contains(&piece.style());
            if code {
                out.push(text_pad(self.pad, " ", piece.style()));
            }
            out.push(*piece);
            if code {
                out.push(text_pad(self.pad, " ", piece.style()));
            }
            if self.src.osc8 || piece.source() != Source::Text {
                continue;
            }
            let Some(link) = links
                .iter()
                .find(|link| link.label.end == piece.end() as u32)
            else {
                continue;
            };
            if link.dest.range().is_empty() {
                continue;
            }
            out.push(text_pad(self.pad, " (", 0));
            out.push(link.dest);
            out.push(text_pad(self.pad, ")", 0));
        }
    }

    /// Draw one logical line of `pieces` on as many rows as it needs.
    fn draw(
        &mut self,
        line: &LLine,
        pieces: &[Piece],
        out: &mut Rows,
        visible_end: usize,
        mark: Option<&mut TailMark>,
    ) {
        self.draw_from(line, pieces, out, visible_end, Resume::default(), mark);
    }

    /// Draw a line from the row a layout before it stopped at, when `resume`
    /// says where that row is, so a delta re-lays the row it lands on rather
    /// than every row of the line.
    fn draw_from(
        &mut self,
        line: &LLine,
        pieces: &[Piece],
        out: &mut Rows,
        visible_end: usize,
        resume: Resume,
        mut mark: Option<&mut TailMark>,
    ) {
        #[cfg(test)]
        LAID.with(|laid| laid.set(laid.get() + 1));
        let dim = self.styles.intern(dim());
        let mut wrap = Wrap {
            pieces,
            text: self.src.text,
            lit: self.src.lit,
            visible_end,
            piece: resume.piece,
            off: resume.off,
            code: line.kind == Kind::Code,
        };
        let mut first = !resume.cont;
        let mut last: Option<Resume> = None;
        loop {
            if line.kind != Kind::Rule && wrap.done(self.pad) {
                break;
            }
            let start = (out.count(), self.pad.len(), wrap.piece, wrap.off);
            let row = out.row();
            let used = self.prefix(line, !first, dim, row);
            if line.kind == Kind::Rule {
                let fill = (self.width as usize).saturating_sub(used);
                row.push(rule(self.pad, fill, dim));
                break;
            }
            let avail = (self.width as usize).saturating_sub(used).max(1);
            wrap.fill(row, avail, self.pad);
            trim(row, self.src, self.pad);
            last = Some(Resume {
                rows: start.0,
                pad: start.1,
                piece: start.2,
                off: start.3,
                byte: 0,
                cont: !first,
            });
            first = false;
        }
        if let (Some(mark), Some(last)) = (mark.as_mut(), last) {
            **mark = TailMark {
                line: mark.line,
                first: mark.first,
                resume: last,
                ended: mark.ended,
            };
        }
    }

    /// Draw the left margin of a line and return its width.
    fn prefix(&mut self, line: &LLine, cont: bool, dim: u32, row: &mut Vec<Piece>) -> usize {
        let quote = self.styles.intern(self.roles.quote);
        let mut used = 0;
        for _ in 0..line.quote {
            row.push(text_pad(self.pad, "▎ ", quote));
            used += 2;
        }
        if line.indent > 0 {
            let width = line.indent as usize;
            row.push(spaces(self.pad, width, 0));
            used += width;
        }
        let marker = line.marker;
        let marker_width = marker.width();
        if marker_width > 0 {
            used += marker_width;
            if cont {
                row.push(spaces(self.pad, marker_width, 0));
            } else {
                row.push(marker_piece(marker, self.pad, dim));
            }
        }
        used
    }

    fn table(&mut self, table: &Table, out: &mut Rows, scratch: &mut Vec<Piece>) {
        let columns = table
            .head
            .len()
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        if self.gap {
            out.row();
            self.gap = false;
        }
        let mut widths = vec![0; columns];
        for (i, cell) in table.head.iter().enumerate() {
            widths[i] = widths[i].max(self.cell_width(cell));
        }
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < columns {
                    widths[i] = widths[i].max(self.cell_width(cell));
                }
            }
        }
        let total: usize = widths.iter().sum::<usize>() + ROW_GAP * (columns - 1);
        if total <= self.width as usize {
            let dim = self.styles.intern(dim());
            if !table.head.is_empty() {
                self.grid_row(&table.head, &widths, out);
                out.row().push(rule(self.pad, total, dim));
            }
            for row in &table.rows {
                self.grid_row(row, &widths, out);
            }
        } else {
            self.pairs(table, out, scratch);
        }
    }

    fn cell_width(&self, cell: &Cell) -> usize {
        cell.pieces
            .iter()
            .map(|piece| width(self.text_of(piece)))
            .sum()
    }

    /// The text of one piece, which for a layout piece is in the pad buffer.
    fn text_of(&self, piece: &Piece) -> &str {
        let text = match piece.source() {
            Source::Text => self.src.text,
            Source::Lit => self.src.lit,
            Source::Pad => self.pad,
        };
        &text[piece.start()..piece.end()]
    }

    fn grid_row(&mut self, cells: &[Cell], widths: &[usize], out: &mut Rows) {
        let row = out.row();
        for (column, cell) in cells.iter().enumerate() {
            let width = widths[column];
            let used = self.cell_width(cell).min(width);
            if column > 0 {
                row.push(spaces(self.pad, ROW_GAP, 0));
            }
            row.extend_from_slice(&cell.pieces);
            if used < width {
                row.push(spaces(self.pad, width - used, 0));
            }
        }
        trim(row, self.src, self.pad);
    }

    /// A table wider than the width: one `header: value` line per cell, wrapped.
    fn pairs(&mut self, table: &Table, out: &mut Rows, scratch: &mut Vec<Piece>) {
        let dim = self.styles.intern(dim());
        let mut line = LLine::default();
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                if cell.pieces.is_empty() {
                    continue;
                }
                scratch.clear();
                if let Some(head) = table.head.get(i).filter(|head| !head.pieces.is_empty()) {
                    scratch.extend_from_slice(&head.pieces);
                    scratch.push(text_pad(self.pad, ": ", dim));
                }
                scratch.extend_from_slice(&cell.pieces);
                std::mem::swap(&mut line.pieces, scratch);
                self.draw(&line, &line.pieces, out, usize::MAX, None);
                std::mem::swap(&mut line.pieces, scratch);
            }
        }
    }
}

fn dim() -> Style {
    Style {
        dim: true,
        ..Style::default()
    }
}

/// The message text of a code line and where it starts, when its pieces are one
/// contiguous run of the message.
fn code_text<'l>(line: &LLine, src: &'l Src<'_>) -> Option<(usize, &'l str)> {
    let start = line.pieces.first()?.start();
    let mut end = start;
    for piece in &line.pieces {
        if piece.source() != Source::Text || piece.start() != end {
            return None;
        }
        end = piece.end();
    }
    Some((start, src.text.get(start..end)?))
}

/// Whether a code line has ended: the byte after its text is the line's end.
fn ended(text: &str, index: usize, count: usize, line: &LLine) -> bool {
    if index + 1 < count {
        return true;
    }
    line.pieces
        .last()
        .is_some_and(|piece| matches!(text.as_bytes().get(piece.end()), Some(b'\n' | b'\r')))
}

/// Where a byte of a line falls in its pieces: the piece and the byte in it.
fn locate(pieces: &[Piece], byte: usize) -> (usize, usize) {
    let mut at = 0;
    for (index, piece) in pieces.iter().enumerate() {
        let len = piece.end() - piece.start();
        if byte < at + len {
            return (index, byte - at);
        }
        at += len;
    }
    (pieces.len().saturating_sub(1), byte.saturating_sub(at))
}

/// The byte of a line where the `piece` at `off` starts.
fn byte_of(pieces: &[Piece], piece: usize, off: usize) -> usize {
    pieces
        .iter()
        .take(piece)
        .map(|piece| piece.end() - piece.start())
        .sum::<usize>()
        + off
}

fn marker_piece(marker: Marker, pad: &mut String, style: u32) -> Piece {
    let start = pad.len();
    match marker {
        Marker::None => {}
        Marker::Bullet => pad.push_str("• "),
        Marker::Ordered(number) => {
            let _ = write!(pad, "{number}. ");
        }
        Marker::Task(true) => pad.push_str("[x] "),
        Marker::Task(false) => pad.push_str("[ ] "),
    }
    Piece::new(Source::Pad, start, pad.len(), style)
}

fn text_pad(pad: &mut String, text: &str, style: u32) -> Piece {
    let start = pad.len();
    pad.push_str(text);
    Piece::new(Source::Pad, start, pad.len(), style)
}

fn spaces(pad: &mut String, count: usize, style: u32) -> Piece {
    let start = pad.len();
    for _ in 0..count {
        pad.push(' ');
    }
    Piece::new(Source::Pad, start, pad.len(), style)
}

fn rule(pad: &mut String, width: usize, style: u32) -> Piece {
    let start = pad.len();
    for _ in 0..width {
        pad.push('─');
    }
    Piece::new(Source::Pad, start, pad.len(), style)
}

/// Drop the trailing spaces of a row.
fn trim(row: &mut Vec<Piece>, src: &Src<'_>, pad: &str) {
    while let Some(last) = row.last() {
        let text = match last.source() {
            Source::Text => &src.text[last.start()..last.end()],
            Source::Lit => &src.lit[last.start()..last.end()],
            Source::Pad => &pad[last.start()..last.end()],
        };
        if text.bytes().all(|byte| byte == b' ') {
            row.pop();
        } else {
            break;
        }
    }
}

/// The width of a string in columns, with the ASCII fast path.
pub(super) fn width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        UnicodeWidthStr::width(text)
    }
}

/// How many bytes of `text` fit in `room` columns. A grapheme is measured as
/// the screen measures it, base and its variation and zero-width followers
/// together, so a column count here is the one the grid draws.
fn fit(text: &str, room: usize) -> usize {
    if text.is_ascii() {
        return room.min(text.len());
    }
    let mut used = 0;
    let mut at = 0;
    while at < text.len() {
        let end = grapheme_end(text, at);
        let columns = UnicodeWidthStr::width(&text[at..end]);
        if used + columns > room {
            break;
        }
        used += columns;
        at = end;
    }
    at
}

/// The end of the grapheme that starts at `at`: the base character and the
/// zero-width or variation characters that follow it.
fn grapheme_end(text: &str, at: usize) -> usize {
    let mut end = at + first_char(&text[at..]);
    while let Some(c) = text[end..].chars().next() {
        if UnicodeWidthChar::width(c) != Some(0) {
            break;
        }
        end += c.len_utf8();
    }
    end
}

fn first_char(text: &str) -> usize {
    text.chars().next().map_or(0, char::len_utf8)
}

/// Place the pieces of one line on rows.
struct Wrap<'a> {
    pieces: &'a [Piece],
    text: &'a str,
    lit: &'a str,
    visible_end: usize,
    piece: usize,
    off: usize,
    code: bool,
}

impl<'a> Wrap<'a> {
    fn rest<'p>(&mut self, pad: &'p str) -> Option<(Piece, &'p str)>
    where
        'a: 'p,
    {
        loop {
            let piece = *self.pieces.get(self.piece)?;
            let start = piece.start() + self.off;
            let mut end = piece.end();
            if piece.source() == Source::Text {
                end = end.min(self.visible_end);
            }
            if start >= end {
                self.piece += 1;
                self.off = 0;
                continue;
            }
            let text = match piece.source() {
                Source::Lit => self.lit,
                Source::Text => self.text,
                Source::Pad => pad,
            };
            return Some((piece, &text[start..end]));
        }
    }

    /// Whether nothing is left to place, spaces at the start of a row included.
    fn done(&mut self, pad: &str) -> bool {
        loop {
            let Some((_, rest)) = self.rest(pad) else {
                return true;
            };
            if !self.code && rest.as_bytes()[0] == b' ' {
                let n = rest.bytes().take_while(|byte| *byte == b' ').count();
                self.off += n;
                continue;
            }
            return false;
        }
    }

    /// Place as much as fits on one row that has `avail` columns left after the
    /// prefix the row already carries. Answers the columns placed here, so the
    /// prefix is not counted twice.
    fn fill(&mut self, row: &mut Vec<Piece>, avail: usize, pad: &str) -> usize {
        let mut used = 0;
        let mut placed = false;
        let mut mark: Option<usize> = None;
        loop {
            let Some((piece, rest)) = self.rest(pad) else {
                return used;
            };
            if !self.code && rest.as_bytes()[0] == b' ' {
                let n = rest.bytes().take_while(|byte| *byte == b' ').count();
                if placed {
                    mark = Some(row.len());
                    row.push(piece.slice(self.off, n));
                    used += n;
                }
                self.off += n;
                continue;
            }
            let n = if self.code {
                rest.len()
            } else {
                rest.find(' ').unwrap_or(rest.len())
            };
            let columns = width(&rest[..n]);
            if placed && used + columns > avail {
                if let Some(at) = mark {
                    row.truncate(at);
                }
                return used;
            }
            let room = avail.saturating_sub(used).max(1);
            if columns > room {
                let mut fits = fit(&rest[..n], room);
                if fits == 0 && placed {
                    return used;
                }
                if fits == 0 {
                    fits = first_char(rest);
                }
                row.push(piece.slice(self.off, fits));
                used += width(&rest[..fits]);
                self.off += fits;
                return used;
            }
            row.push(piece.slice(self.off, n));
            used += columns;
            placed = true;
            self.off += n;
            mark = None;
        }
    }
}
