//! Turn a message's text into top-level blocks of spans.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::{
    BOLD, Block, Body, CODE, Cell, ITALIC, Kind, LINK, LLine, Link, Marker, Piece, Roles, STRIKE,
    Source, StyleTable, Table,
};
use crate::screen::Style;

/// A table of the GFM extensions the renderer understands; no footnotes, math
/// or HTML rendering.
fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// The buffers a parse fills and the styles it interns.
pub(super) struct Draft<'a> {
    /// The literal text the parser generates.
    pub lit: &'a mut String,
    /// The interned styles the spans refer to.
    pub styles: &'a mut StyleTable,
    /// The styles of the blocks that have a role of their own.
    pub roles: Roles,
    /// Set when a span had to be cut at the end of the message.
    pub clamped: bool,
}

/// The blocks of the text from `base` on, whose spans are byte ranges into the
/// message. `text` is that text plus any closer a streamed marker still waits
/// for, and `real` is the message's length, so a span never covers a closer that
/// the message does not hold.
pub(super) fn parse<'a>(
    text: &'a str,
    base: usize,
    real: usize,
    draft: &mut Draft<'a>,
) -> Vec<Block> {
    let lit_start = draft.lit.len();
    let mut state = Parse {
        text,
        base,
        real,
        draft,
        blocks: Vec::new(),
        lines: Vec::new(),
        cur: LLine::default(),
        block_start: base,
        lit_start,
        depth: 0,
        quote: 0,
        bits: 0,
        level: 0,
        head: false,
        code: false,
        list: false,
        fence: None,
        lists: Vec::new(),
        items: Vec::new(),
        pending: None,
        table: None,
        row: Vec::new(),
        links: Vec::new(),
        link: None,
    };
    for (event, range) in Parser::new_ext(text, options()).into_offset_iter() {
        state.event(event, base + range.start, base + range.end);
    }
    state.close(base + text.len());
    state.blocks
}

/// One list of the nesting.
struct Level {
    next: Option<u64>,
}

/// One item of the nesting.
struct Item {
    indent: u16,
    cont: u16,
}

/// A link whose label is still being collected.
struct LinkDraft {
    label: Option<Range<u32>>,
    dest: Piece,
}

struct Parse<'a, 'b> {
    /// The text this parse reads: the message from `base` on, plus the closers.
    text: &'a str,
    /// Where `text` starts in the message.
    base: usize,
    /// The message's length: a span past it is a closer, not text.
    real: usize,
    draft: &'b mut Draft<'a>,
    blocks: Vec<Block>,
    lines: Vec<LLine>,
    cur: LLine,
    block_start: usize,
    lit_start: usize,
    depth: usize,
    quote: u8,
    bits: u16,
    level: u16,
    head: bool,
    code: bool,
    list: bool,
    fence: Option<u8>,
    lists: Vec<Level>,
    items: Vec<Item>,
    pending: Option<Marker>,
    table: Option<Table>,
    row: Vec<Cell>,
    links: Vec<Link>,
    link: Option<LinkDraft>,
}

impl Parse<'_, '_> {
    fn event(&mut self, event: Event<'_>, start: usize, end: usize) {
        match event {
            Event::Start(tag) => {
                if self.depth == 0 {
                    self.begin(start);
                }
                self.depth += 1;
                self.start(tag, start, end);
            }
            Event::End(tag) => {
                self.end(tag);
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.close(end);
                }
            }
            Event::Text(text) => self.text(&text, start),
            Event::Code(text) => {
                let style = self.code_style();
                let piece = match self.code_range(&text, start, end) {
                    Some(range) => self.literal_styled(&text, range, style),
                    None => self.owned(&text, style),
                };
                self.push(piece.inline_code());
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                let piece = self.literal(&text, start..end);
                self.push(piece);
            }
            Event::SoftBreak => {
                let style = self.style();
                let piece = self.owned(" ", style);
                self.push(piece);
            }
            Event::HardBreak => self.flush(),
            Event::TaskListMarker(checked) => {
                let marker = Marker::Task(checked);
                self.pending = Some(marker);
                if let Some(item) = self.items.last_mut() {
                    item.cont = item.indent + marker.width() as u16;
                }
            }
            Event::Rule => {
                if self.depth == 0 {
                    self.begin(start);
                }
                self.cur.kind = Kind::Rule;
                self.flush();
                if self.depth == 0 {
                    self.close(end);
                }
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, start: usize, end: usize) {
        match tag {
            Tag::Heading { level, .. } => self.level = level as u16,
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.code = true;
                if let CodeBlockKind::Fenced(info) = kind {
                    let head = self.at(start, end).trim_start_matches(' ');
                    self.fence = head.bytes().next().filter(|b| *b == b'`' || *b == b'~');
                    if !info.is_empty() {
                        let line_end = self.at(start, end).find('\n').map_or(end, |at| start + at);
                        let style = self.draft.styles.intern(dim());
                        let piece = match self.at(start, line_end).find(info.as_ref()) {
                            Some(at) => self.span(start + at, start + at + info.len(), style),
                            None => self.owned(&info, style),
                        };
                        self.lines.push(LLine {
                            quote: self.quote,
                            indent: self.content_indent(),
                            kind: Kind::Text,
                            pieces: vec![piece],
                            ..LLine::default()
                        });
                    }
                }
            }
            Tag::List(first) => {
                self.flush();
                if self.depth == 1 {
                    self.list = true;
                }
                self.lists.push(Level { next: first });
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1) as u16;
                let indent = depth * 2;
                let marker = match self.lists.last_mut() {
                    Some(Level { next: Some(n) }) => {
                        let marker = Marker::Ordered(*n as u32);
                        *n += 1;
                        marker
                    }
                    _ => Marker::Bullet,
                };
                self.items.push(Item {
                    indent,
                    cont: indent + marker.width() as u16,
                });
                self.pending = Some(marker);
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(Table {
                    head: Vec::new(),
                    rows: Vec::new(),
                });
            }
            Tag::TableHead => self.head = true,
            Tag::TableRow | Tag::TableCell => {}
            Tag::Strong => self.bits |= BOLD,
            Tag::Emphasis => self.bits |= ITALIC,
            Tag::Strikethrough => self.bits |= STRIKE,
            Tag::Link { dest_url, .. } => {
                self.bits |= LINK;
                let dest = match self.at(start, end).rfind(dest_url.as_ref()) {
                    Some(at) => self.span(start + at, start + at + dest_url.len(), 0),
                    None => self.owned(&dest_url, 0),
                };
                self.link = Some(LinkDraft { label: None, dest });
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.level = 0;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if self.cur.kind == Kind::Code || !self.cur.pieces.is_empty() {
                    self.flush();
                }
                self.code = false;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
                self.pending = None;
            }
            TagEnd::TableHead => {
                self.head = false;
                self.finish_row(true);
            }
            TagEnd::TableRow => self.finish_row(false),
            TagEnd::TableCell => {
                let pieces = std::mem::take(&mut self.cur.pieces);
                self.row.push(Cell { pieces });
            }
            TagEnd::Strong => self.bits &= !BOLD,
            TagEnd::Emphasis => self.bits &= !ITALIC,
            TagEnd::Strikethrough => self.bits &= !STRIKE,
            TagEnd::Link => {
                self.bits &= !LINK;
                if let Some(LinkDraft {
                    label: Some(label),
                    dest,
                }) = self.link.take()
                {
                    self.links.push(Link { label, dest });
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str, start: usize) {
        if self.code {
            self.cur.kind = Kind::Code;
        }
        let mut off = 0;
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.flush();
                if self.code {
                    self.cur.kind = Kind::Code;
                }
            }
            if !part.is_empty() {
                let piece = self.literal(part, start + off..start + off + part.len());
                self.push(piece);
            }
            off += part.len() + 1;
        }
    }

    fn begin(&mut self, start: usize) {
        self.block_start = start;
        self.lit_start = self.draft.lit.len();
        self.lines.clear();
        self.cur = LLine::default();
        self.bits = 0;
        self.level = 0;
        self.head = false;
        self.quote = 0;
        self.code = false;
        self.list = false;
        self.fence = None;
        self.pending = None;
        self.table = None;
        self.lists.clear();
        self.items.clear();
        self.links.clear();
        self.link = None;
    }

    fn close(&mut self, end: usize) {
        self.flush();
        let start = self.block_start.min(self.real);
        let end = end.min(self.real);
        let body = match self.table.take() {
            Some(table) if !table.head.is_empty() || !table.rows.is_empty() => Body::Table(table),
            Some(_) => return,
            None => {
                if self.lines.is_empty() {
                    return;
                }
                Body::Flow(std::mem::take(&mut self.lines))
            }
        };
        let block = Block {
            start,
            end,
            lit: self.lit_start..self.draft.lit.len(),
            body,
            links: std::mem::take(&mut self.links),
            fence: self.fence,
            list: self.list,
            line: 0,
            rows: 0,
        };
        self.blocks.push(block);
    }

    fn flush(&mut self) {
        let mut line = std::mem::take(&mut self.cur);
        if line.pieces.is_empty() && matches!(line.kind, Kind::Text) {
            self.cur = line;
            return;
        }
        line.quote = self.quote;
        let marker = self.pending.take();
        line.marker = marker.unwrap_or_default();
        line.indent = match (marker, self.items.last()) {
            (Some(_), Some(item)) => item.indent,
            (_, Some(item)) => item.cont,
            (_, None) => 0,
        };
        self.lines.push(line);
    }

    fn content_indent(&self) -> u16 {
        self.items.last().map_or(0, |item| item.cont)
    }

    fn finish_row(&mut self, header: bool) {
        let cells = std::mem::take(&mut self.row);
        let table = self.table.as_mut();
        if let Some(table) = table {
            if header {
                table.head = cells;
            } else if !cells.is_empty() {
                table.rows.push(cells);
            }
        }
    }

    fn push(&mut self, piece: Piece) {
        if let Some(link) = self
            .link
            .as_mut()
            .filter(|_| piece.source() == Source::Text)
        {
            match &mut link.label {
                Some(label) => label.end = piece.end() as u32,
                None => link.label = Some(piece.start() as u32..piece.end() as u32),
            }
        }
        self.cur.pieces.push(piece);
    }

    fn style(&mut self) -> u32 {
        let bits = self.bits | if self.head { BOLD } else { 0 } | if self.code { CODE } else { 0 };
        let quote = self.quote > 0 && !self.code;
        let style = self.draft.roles.style(bits, self.level, quote);
        self.draft.styles.intern(style)
    }

    fn code_style(&mut self) -> u32 {
        let bits = self.bits | CODE | if self.head { BOLD } else { 0 };
        let style = self.draft.roles.style(bits, self.level, false);
        self.draft.styles.intern(style)
    }

    /// A piece for parser text that is a slice of the message when it is one.
    fn literal(&mut self, text: &str, range: Range<usize>) -> Piece {
        let style = self.style();
        self.literal_styled(text, range, style)
    }

    fn literal_styled(&mut self, text: &str, range: Range<usize>, style: u32) -> Piece {
        let from = range.start.saturating_sub(self.base);
        let to = range.end.saturating_sub(self.base);
        let Some(slice) = self.text.get(from..to) else {
            return self.owned(text, style);
        };
        if slice == text {
            return self.span(range.start, range.end, style);
        }
        if let Some(at) = slice.find(text) {
            return self.span(range.start + at, range.start + at + text.len(), style);
        }
        self.owned(text, style)
    }

    /// A span of the message at an absolute range, cut at the message's end so a
    /// closer never shows as text.
    fn span(&mut self, start: usize, end: usize, style: u32) -> Piece {
        if end > self.real {
            self.draft.clamped = true;
        }
        let start = start.min(self.real);
        let end = end.min(self.real).max(start);
        Piece::new(Source::Text, start, end, style)
    }

    /// The message text of an absolute range.
    fn at(&self, start: usize, end: usize) -> &str {
        let from = start.saturating_sub(self.base);
        let to = end.saturating_sub(self.base);
        self.text.get(from..to).unwrap_or_default()
    }

    /// The range of a code span's text inside the span, without its backticks.
    fn code_range(&self, inner: &str, start: usize, end: usize) -> Option<Range<usize>> {
        let at = self.at(start, end).find(inner)?;
        Some(start + at..start + at + inner.len())
    }

    /// A piece for parser text that is not a slice of the message.
    fn owned(&mut self, text: &str, style: u32) -> Piece {
        let start = self.draft.lit.len();
        self.draft.lit.push_str(text);
        Piece::new(Source::Lit, start, self.draft.lit.len(), style)
    }
}

fn dim() -> Style {
    Style {
        dim: true,
        ..Style::default()
    }
}
