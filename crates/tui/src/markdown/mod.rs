//! Streamed markdown: finished blocks rendered once, the open block parsed again
//! as text arrives.
//!
//! `push` appends a delta to one owned buffer and lays out only the open block.
//! A block is parsed once with `pulldown-cmark` when the parser has moved past
//! it, and its spans are byte ranges into that buffer, so a finished block never
//! copies text and never parses again. A width change re-lays out every block
//! without a parse.
//!
//! The open block's tail is where a stream spends most of its deltas, so it has
//! two shortcuts that keep a delta's cost on the delta: a plain delta only grows
//! the last span and re-lays the last line, and text inside an open fenced code
//! block is appended without a parse at all.
//!
//! While the block is open, a marker whose closer has not streamed yet is closed
//! in a scratch copy of the block, so `Some **b` draws `Some b` rather than its
//! markers. A reference link resolves only when its definition is in the same
//! block: streaming keeps it as written (`[docs][1]`) when the definition comes
//! in a later block, since that block is finished before the definition
//! arrives.
//!
//! An owned buffer beats re-supplying an `Arc<str>` with a longer length: the
//! deltas arrive as `&str` pieces of the stream, so appending copies each byte
//! once, where re-supplying would copy the whole message per delta.

mod layout;
mod parse;

#[cfg(test)]
mod tests;

#[cfg(test)]
use std::cell::RefCell;
use std::ops::Range;

use crate::highlight::Token;
use crate::screen::{Color, Style, StyleTable};
use crate::theme::{Role, Theme};

/// Inline bold.
const BOLD: u16 = 1 << 0;
/// Inline italic.
const ITALIC: u16 = 1 << 1;
/// Inline strikethrough.
const STRIKE: u16 = 1 << 2;
/// Inline code.
const CODE: u16 = 1 << 3;
/// A link label.
const LINK: u16 = 1 << 4;

/// The tokens of a code block, in the order [`Roles::tokens`] holds their
/// styles.
const TOKENS: [Token; 11] = [
    Token::Text,
    Token::Keyword,
    Token::String,
    Token::Comment,
    Token::Number,
    Token::Type,
    Token::Function,
    Token::Macro,
    Token::Punct,
    Token::Added,
    Token::Removed,
];

/// The role each of [`TOKENS`] draws with.
const TOKEN_ROLES: [Role; TOKENS.len()] = [
    Role::Text,
    Role::CodeKeyword,
    Role::CodeString,
    Role::CodeComment,
    Role::CodeNumber,
    Role::CodeType,
    Role::CodeFunction,
    Role::CodeMacro,
    Role::CodePunct,
    Role::Add,
    Role::Delete,
];

/// The styles of the blocks that have a role of their own: a heading's text, a
/// quote's bar, the fill behind inline code, the fill of a code block and the
/// colors of its tokens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Roles {
    heading: Style,
    quote: Style,
    code: Style,
    /// The fill a code block draws on.
    block: Style,
    /// The fence's tag: dim, on the block's fill.
    label: Style,
    /// The styles of the code tokens, in [`TOKENS`] order.
    tokens: [Style; TOKENS.len()],
}

impl Default for Roles {
    fn default() -> Self {
        let block = Style {
            bg: Color::Indexed(236),
            ..Style::default()
        };
        Roles {
            heading: Style {
                fg: Color::Indexed(6),
                bold: true,
                ..Style::default()
            },
            quote: Style {
                fg: Color::Indexed(6),
                ..Style::default()
            },
            code: block,
            block,
            label: Style {
                dim: true,
                bg: block.bg,
                ..Style::default()
            },
            tokens: [block; TOKENS.len()],
        }
    }
}

impl Roles {
    /// The roles of a resolved theme.
    fn of(theme: &Theme) -> Self {
        let block = Style {
            bg: theme.fill(Role::CodeFill),
            ..Style::default()
        };
        let mut tokens = [Style::default(); TOKENS.len()];
        for (style, role) in tokens.iter_mut().zip(TOKEN_ROLES) {
            *style = Style {
                bg: block.bg,
                ..theme.style(role)
            };
        }
        Roles {
            heading: theme.style(Role::Heading),
            quote: theme.style(Role::QuoteBar),
            code: block,
            block,
            label: Style {
                bg: block.bg,
                ..theme.style(Role::Dim)
            },
            tokens,
        }
    }

    /// The style of a code token.
    fn token(&self, token: Token) -> Style {
        self.tokens[token as usize]
    }

    /// The style of an inline bit set at a heading level, in a quote or both;
    /// level 0 is body text.
    fn style(&self, bits: u16, level: u16, quote: bool) -> Style {
        let mut style = Style::default();
        if bits & BOLD != 0 {
            style.bold = true;
        }
        if bits & ITALIC != 0 {
            style.italic = true;
        }
        if bits & STRIKE != 0 {
            style.strikethrough = true;
        }
        if bits & CODE != 0 {
            style.bg = self.code.bg;
        }
        if bits & LINK != 0 {
            style.underline = true;
            style.fg = Color::Indexed(39);
        }
        if level != 0 {
            style.bold = true;
            style.fg = self.heading.fg;
        } else if quote {
            style.italic = true;
            style.dim = true;
        }
        style
    }
}

/// Where the text of a piece lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// A range of the message text.
    Text,
    /// A range of the literal text the parser generated: a soft break, a link
    /// destination, or text whose markup had to be decoded.
    Lit,
    /// A range of the layout text: indents, quote bars, bullets, numbers, table
    /// fill.
    Pad,
}

/// A styled run of a rendered line: a byte range into the message, the literal
/// or the layout text, and an interned style id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Piece {
    source: Source,
    start: u32,
    end: u32,
    style: u32,
}

impl Piece {
    pub(crate) fn new(source: Source, start: usize, end: usize, style: u32) -> Self {
        Piece {
            source,
            start: start as u32,
            end: end as u32,
            style,
        }
    }

    /// Where the text lives.
    pub fn source(&self) -> Source {
        self.source
    }

    /// The byte range of the text.
    pub fn range(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    /// The start of the range.
    pub(crate) fn start(&self) -> usize {
        self.start as usize
    }

    /// The end of the range.
    pub(crate) fn end(&self) -> usize {
        self.end as usize
    }

    /// The interned style id.
    pub fn style(&self) -> u32 {
        self.style
    }

    /// The same run moved `off` bytes into its text, `len` bytes long.
    pub(crate) fn slice(&self, off: usize, len: usize) -> Piece {
        Piece {
            source: self.source,
            start: (self.start() + off) as u32,
            end: (self.start() + off + len) as u32,
            style: self.style,
        }
    }
}

/// How a logical line is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Kind {
    /// Text: wrapped at spaces, and a trailing run of markup characters is held
    /// back while the block is open.
    #[default]
    Text,
    /// A code line: wrapped at the column, never held back.
    Code,
    /// A horizontal rule: filled to the width.
    Rule,
}

/// What is drawn before a line's text.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Marker {
    /// Nothing.
    #[default]
    None,
    /// A bullet.
    Bullet,
    /// The number of an ordered item.
    Ordered(u32),
    /// The box of a task item.
    Task(bool),
}

impl Marker {
    /// Columns the marker takes.
    fn width(self) -> usize {
        match self {
            Marker::None => 0,
            Marker::Bullet => 2,
            Marker::Task(_) => 4,
            Marker::Ordered(n) => digits(n) + 2,
        }
    }
}

fn digits(mut n: u32) -> usize {
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

/// One unwrapped rendered line of a block.
#[derive(Clone, Debug, Default)]
struct LLine {
    quote: u8,
    indent: u16,
    marker: Marker,
    kind: Kind,
    pieces: Vec<Piece>,
}

/// One table cell: the spans of its text.
#[derive(Clone, Debug, Default)]
struct Cell {
    pieces: Vec<Piece>,
}

/// A parsed table: the header cells and the body rows.
#[derive(Clone, Debug)]
struct Table {
    head: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
}

/// A link of the message: the byte range of its label and its destination.
#[derive(Clone, Debug)]
struct Link {
    label: Range<u32>,
    dest: Piece,
}

/// The rendered form of a parsed block.
#[derive(Clone, Debug)]
enum Body {
    Flow(Vec<LLine>),
    Table(Table),
}

/// One top-level block of the message.
#[derive(Clone, Debug)]
struct Block {
    start: usize,
    end: usize,
    lit: Range<usize>,
    body: Body,
    links: Vec<Link>,
    fence: Option<u8>,
    list: bool,
}

impl Block {
    /// The last line of a flow block, for the holdback rules.
    fn last_line(&self) -> Option<&LLine> {
        match &self.body {
            Body::Flow(lines) => lines.last(),
            Body::Table(_) => None,
        }
    }
}

/// Rendered lines, reusing the piece vector of every row it drew before so a
/// steady stream does not allocate per delta. Rows past `count` stay allocated
/// but are not shown.
#[derive(Debug, Default)]
struct Rows {
    lines: Vec<Line>,
    count: usize,
}

impl Rows {
    fn reset(&mut self) {
        self.count = 0;
    }

    /// How many rows are drawn.
    fn count(&self) -> usize {
        self.count
    }

    /// Forget the rows from `count` on, keeping their buffers.
    fn truncate(&mut self, count: usize) {
        self.count = count.min(self.lines.len());
    }

    fn row(&mut self) -> &mut Vec<Piece> {
        if self.count == self.lines.len() {
            self.lines.push(Line::default());
        }
        let row = &mut self.lines[self.count].pieces;
        row.clear();
        self.count += 1;
        row
    }

    fn drawn(&self) -> &[Line] {
        &self.lines[..self.count]
    }

    fn drop_last(&mut self) {
        self.count = self.count.saturating_sub(1);
    }
}

/// Where a row of the last line starts: the rows and the pad of the layout
/// before it, the span and the byte in it, and whether its margin is a
/// continuation's. A code line names its byte in the line instead, since the
/// tokens of a line are found again when it ends.
#[derive(Clone, Copy, Debug, Default)]
struct Resume {
    rows: usize,
    pad: usize,
    piece: usize,
    off: usize,
    byte: usize,
    cont: bool,
}

/// Where the last line of the last open block starts, so a delta that only
/// touches that line re-lays one row of it instead of the whole block.
#[derive(Clone, Copy, Debug, Default)]
struct TailMark {
    line: usize,
    /// Where the line's first row starts, so a line whose tokens changed while
    /// it was open is drawn again whole.
    first: Resume,
    resume: Resume,
    /// Whether the line had ended when the mark was set.
    ended: bool,
}

/// One rendered line: its pieces, in display order.
#[derive(Clone, Debug, Default)]
pub struct Line {
    pieces: Vec<Piece>,
}

impl Line {
    /// The pieces of the line, left to right.
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }
}

/// A message rendered as markdown at a width.
#[derive(Debug)]
pub struct Markdown {
    text: String,
    lit: String,
    pad: String,
    styles: StyleTable,
    roles: Roles,
    /// The interned styles of inline code: a layout fills a space on each side
    /// of a span that carries one.
    codes: Vec<u32>,
    width: u16,
    osc8: bool,
    blocks: Vec<Block>,
    committed: Rows,
    frozen_to: usize,
    lit_frozen: usize,
    pad_frozen: usize,
    open: Vec<Block>,
    live: Rows,
    /// Where the last line of the open block starts in the live rows and pad.
    tail: TailMark,
    /// The tokenizer of the code blocks: the state it carries from line to line
    /// and the span buffer it fills.
    highlight: layout::Highlighter,
    /// Whether the last parse of the open block clamped a span, so the last
    /// line's last span is not the running text and must be parsed again.
    tail_literal: bool,
    scratch: Vec<Piece>,
    finished: bool,
    #[cfg(test)]
    parsed: RefCell<Vec<Range<usize>>>,
}

impl Markdown {
    /// A renderer for one message, `width` columns wide and empty.
    pub fn new(width: u16) -> Self {
        Markdown {
            text: String::new(),
            lit: String::new(),
            pad: String::new(),
            styles: StyleTable::new(),
            roles: Roles::default(),
            codes: Vec::new(),
            width: width.max(1),
            osc8: false,
            blocks: Vec::new(),
            committed: Rows::default(),
            frozen_to: 0,
            lit_frozen: 0,
            pad_frozen: 0,
            open: Vec::new(),
            live: Rows::default(),
            tail: TailMark::default(),
            highlight: layout::Highlighter::default(),
            tail_literal: false,
            scratch: Vec::new(),
            finished: false,
            #[cfg(test)]
            parsed: RefCell::new(Vec::new()),
        }
    }

    /// Draw the blocks with the roles of `theme`: the heading's text, a quote's
    /// bar and the fill behind inline code. A theme change parses the message
    /// again, since the styles are interned with its spans.
    pub fn set_theme(&mut self, theme: &Theme) {
        let roles = Roles::of(theme);
        if roles == self.roles {
            return;
        }
        self.roles = roles;
        self.restyle();
    }

    /// Append streamed text to the message. A push after [`Markdown::finish`]
    /// does nothing.
    pub fn push(&mut self, delta: &str) {
        if self.finished || delta.is_empty() {
            return;
        }
        let before = self.text.len();
        self.text.push_str(delta);
        if plain(delta) && self.extendable(before) {
            self.extend_last();
            self.lay_tail();
            return;
        }
        if plain(delta) && self.extend_cell_ok(before) {
            self.extend_cell();
            self.lay_tail();
            return;
        }
        if self.extend_fence(before, delta) {
            self.lay_tail();
            return;
        }
        self.reparse();
    }

    /// End the reply: the open block becomes a finished block.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // The closers of the open block are a guess about text that has not
        // arrived; the finished block is parsed from the real text once more.
        self.reparse();
        let open = std::mem::take(&mut self.open);
        for block in open {
            self.freeze(block);
        }
        self.lay_live();
    }

    /// Whether the reply has ended.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// The columns a line may fill.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Re-lay out every block for a new width, without parsing again.
    pub fn set_width(&mut self, width: u16) {
        let width = width.max(1);
        if width == self.width {
            return;
        }
        self.width = width;
        self.relayout();
    }

    /// Whether links carry OSC 8 sequences: when off, a link shows as
    /// `label (url)`.
    pub fn set_osc8(&mut self, on: bool) {
        if on == self.osc8 {
            return;
        }
        self.osc8 = on;
        self.relayout();
    }

    /// Whether links carry OSC 8 sequences.
    pub fn osc8(&self) -> bool {
        self.osc8
    }

    /// The message text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The committed lines (finished blocks, which never change again) and the
    /// live lines of the open block.
    pub fn lines(&self) -> (&[Line], &[Line]) {
        (self.committed.drawn(), self.live.drawn())
    }

    /// The text of one piece.
    pub fn piece_text(&self, piece: &Piece) -> &str {
        let text = match piece.source() {
            Source::Text => &self.text,
            Source::Lit => &self.lit,
            Source::Pad => &self.pad,
        };
        &text[piece.start()..piece.end()]
    }

    /// One line as plain text, styles dropped.
    pub fn line_text(&self, line: &Line) -> String {
        let mut text = String::new();
        for piece in &line.pieces {
            text.push_str(self.piece_text(piece));
        }
        text
    }

    /// The interned styles the pieces refer to.
    pub fn styles(&self) -> &StyleTable {
        &self.styles
    }

    /// The destination of the link whose label covers `piece`, for OSC 8. The
    /// label is the span in the message text, so a screen can emit the sequence
    /// around the pieces of one label.
    pub fn link_of(&self, piece: &Piece) -> Option<&str> {
        if piece.source() != Source::Text {
            return None;
        }
        let start = piece.start() as u32;
        let end = piece.end() as u32;
        let blocks = self.blocks.iter().chain(self.open.iter());
        for link in blocks.flat_map(|block| block.links.iter()) {
            if link.label.start <= start && end <= link.label.end {
                let text = match link.dest.source() {
                    Source::Text => &self.text,
                    Source::Lit => &self.lit,
                    Source::Pad => return None,
                };
                return Some(&text[link.dest.range()]);
            }
        }
        None
    }

    /// Draw every block again with the current roles, from the start of the
    /// message: the styles are interned with the spans a parse makes.
    fn restyle(&mut self) {
        self.styles = StyleTable::new();
        self.codes.clear();
        self.blocks.clear();
        self.committed.reset();
        self.open.clear();
        self.live.reset();
        self.lit.clear();
        self.pad.clear();
        self.frozen_to = 0;
        self.lit_frozen = 0;
        self.pad_frozen = 0;
        self.tail = TailMark::default();
        self.tail_literal = false;
        self.reparse();
        if self.finished {
            for block in std::mem::take(&mut self.open) {
                self.freeze(block);
            }
            self.lay_live();
        }
    }

    fn reparse(&mut self) {
        self.lit.truncate(self.lit_frozen);
        self.pad.truncate(self.pad_frozen);
        let start = self.frozen_to;
        let real = self.text.len();
        let closers = if self.finished {
            String::new()
        } else {
            self.closers(start)
        };
        let mut scratch;
        let clamped;
        let blocks = {
            let Markdown {
                text,
                lit,
                styles,
                roles,
                codes,
                ..
            } = self;
            let tail: &str = if closers.is_empty() {
                &text[start..]
            } else {
                scratch = String::with_capacity(real - start + closers.len());
                scratch.push_str(&text[start..]);
                scratch.push_str(&closers);
                &scratch
            };
            let mut draft = parse::Draft {
                lit,
                styles,
                roles: *roles,
                codes,
                clamped: false,
            };
            let blocks = parse::parse(tail, start, real, &mut draft);
            clamped = draft.clamped;
            blocks
        };
        self.tail_literal = clamped;
        #[cfg(test)]
        self.parsed.borrow_mut().push(start..real);
        let count = blocks.len();
        let mut keep = count.saturating_sub(1);
        if count >= 2
            && matches!(blocks[count - 2].body, Body::Table(_))
            && partial_row(&self.text, &blocks[count - 1])
        {
            keep = count - 2;
        }
        let mut blocks = blocks.into_iter();
        for block in blocks.by_ref().take(keep) {
            self.freeze(block);
        }
        self.open.clear();
        self.open.extend(blocks);
        self.lay_live();
    }

    /// The closers a scratch copy of the open block appends, so a marker whose
    /// closer has not streamed yet does not show as text, the way Vercel's
    /// `remend` completes a half-written line.
    fn closers(&self, start: usize) -> String {
        if let Some(block) = self.open.last() {
            if block.fence.is_some() || matches!(block.body, Body::Table(_)) {
                return String::new();
            }
            return close_markers(&self.text[block.start.max(start)..]);
        }
        // No block is open yet: the last block is what follows the last blank
        // line. A fence has no inline construct to close and a backtick would end
        // it, so its text is left alone.
        let text = &self.text[start..];
        let tail = text[text.rfind("\n\n").map_or(0, |at| at + 2)..].trim_start_matches('\n');
        if tail.starts_with("```") || tail.starts_with("~~~") {
            return String::new();
        }
        close_markers(tail)
    }

    fn freeze(&mut self, block: Block) {
        self.frozen_to = block.end;
        self.lit_frozen = block.lit.end;
        let gap = !self.blocks.is_empty();
        self.lay_block(&block, layout::Hold::default(), false, gap);
        self.pad_frozen = self.pad.len();
        self.blocks.push(block);
    }

    fn lay_block(&mut self, block: &Block, hold: layout::Hold, live: bool, gap: bool) {
        let Markdown {
            text,
            lit,
            pad,
            styles,
            roles,
            codes,
            osc8,
            width,
            committed,
            live: live_rows,
            scratch,
            highlight,
            ..
        } = self;
        let src = layout::Src {
            text,
            lit,
            osc8: *osc8,
        };
        let out = if live { live_rows } else { committed };
        layout::Lay {
            width: *width,
            src: &src,
            styles,
            roles: *roles,
            codes: &*codes,
            pad,
            highlight,
            gap,
        }
        .block(block, out, scratch, hold);
    }

    fn lay_live(&mut self) {
        self.pad.truncate(self.pad_frozen);
        self.live.reset();
        let hold = self.hold();
        let Markdown {
            text,
            lit,
            pad,
            styles,
            roles,
            codes,
            osc8,
            width,
            live,
            scratch,
            open,
            tail,
            blocks,
            highlight,
            ..
        } = self;
        let src = layout::Src {
            text,
            lit,
            osc8: *osc8,
        };
        let last = open.len().saturating_sub(1);
        let mut lay = layout::Lay {
            width: *width,
            src: &src,
            styles,
            roles: *roles,
            codes: &*codes,
            pad,
            highlight,
            gap: false,
        };
        let committed = !blocks.is_empty();
        for (i, block) in open.iter().enumerate() {
            let hold = if i == last {
                hold
            } else {
                layout::Hold::default()
            };
            let mark = if i == last { Some(&mut *tail) } else { None };
            lay.gap = committed || i > 0;
            lay.lines(block, 0, live, scratch, hold, mark);
        }
        if hold.hide_last {
            live.drop_last();
        }
    }

    /// Lay out the last line of the open block again, keeping every row and pad
    /// the layout before it drew: what a delta that only touches the tail needs.
    fn lay_tail(&mut self) {
        let hold = self.hold();
        let Markdown {
            text,
            lit,
            pad,
            styles,
            roles,
            codes,
            osc8,
            width,
            live,
            scratch,
            open,
            tail,
            highlight,
            ..
        } = self;
        let src = layout::Src {
            text,
            lit,
            osc8: *osc8,
        };
        let mut lay = layout::Lay {
            width: *width,
            src: &src,
            styles,
            roles: *roles,
            codes: &*codes,
            pad,
            highlight,
            gap: false,
        };
        if let Some(block) = open.last() {
            lay.tail(block, tail, live, scratch, hold);
        }
        if hold.hide_last {
            live.drop_last();
        }
    }

    fn relayout(&mut self) {
        self.pad.clear();
        self.committed.reset();
        {
            let Markdown {
                text,
                lit,
                pad,
                styles,
                roles,
                codes,
                osc8,
                width,
                blocks,
                committed,
                scratch,
                highlight,
                ..
            } = self;
            let src = layout::Src {
                text,
                lit,
                osc8: *osc8,
            };
            let mut lay = layout::Lay {
                width: *width,
                src: &src,
                styles,
                roles: *roles,
                codes: &*codes,
                pad,
                highlight,
                gap: false,
            };
            for (i, block) in blocks.iter().enumerate() {
                lay.gap = i > 0;
                lay.block(block, committed, scratch, layout::Hold::default());
            }
        }
        self.pad_frozen = self.pad.len();
        self.lay_live();
    }

    /// What the open block holds back until more text arrives.
    fn hold(&self) -> layout::Hold {
        let Some(open) = self.open.last() else {
            return layout::Hold::default();
        };
        if self.finished {
            return layout::Hold::default();
        }
        let text = self.text.as_str();
        let partial_fence = partial_fence(text, open);
        let trims = !partial_fence
            && open.fence.is_none()
            && open.last_line().is_some_and(|line| line.kind == Kind::Text);
        layout::Hold {
            visible_end: if trims { visible_end(text) } else { usize::MAX },
            hide_last: open.list && !text.ends_with('\n'),
            pending_table: pending_table(text, open),
            partial_fence,
        }
    }

    #[cfg(test)]
    fn parses(&self) -> usize {
        self.parsed.borrow().len()
    }

    #[cfg(test)]
    fn parse_ranges(&self) -> Vec<Range<usize>> {
        self.parsed.borrow().clone()
    }

    #[cfg(test)]
    fn frozen_end(&self) -> usize {
        self.frozen_to
    }

    /// Whether a plain delta only extends the last span, so the open block keeps
    /// every span the last parse gave it.
    fn extendable(&self, at: usize) -> bool {
        if self.tail_literal {
            return false;
        }
        let Some(Block {
            body: Body::Flow(lines),
            ..
        }) = self.open.last()
        else {
            return false;
        };
        let Some(line) = lines.last() else {
            return false;
        };
        if !matches!(line.kind, Kind::Text | Kind::Code) {
            return false;
        }
        let Some(piece) = line.pieces.last() else {
            return false;
        };
        // A marker run the parser left as text must not grow into real text: the
        // next delta can open the construct the run waits for.
        if line.kind == Kind::Text && at > visible_end(&self.text[..at]) {
            return false;
        }
        self.tail_span(piece, at)
    }

    /// Whether `piece` is the running text that a delta can grow: it ends at
    /// `at`, or the bytes between are the spaces the parser left out of the last
    /// span of a line.
    fn tail_span(&self, piece: &Piece, at: usize) -> bool {
        if piece.source() != Source::Text || piece.end() > at {
            return false;
        }
        self.text[piece.end()..at].bytes().all(|byte| byte == b' ')
    }

    /// Grow the last span of the open block's last line over the new text.
    fn extend_last(&mut self) {
        let end = self.text.len();
        let Some(piece) = self
            .open
            .last_mut()
            .and_then(|block| match &mut block.body {
                Body::Flow(lines) => lines.last_mut().and_then(|line| line.pieces.last_mut()),
                Body::Table(_) => None,
            })
        else {
            return;
        };
        piece.end = end as u32;
    }

    /// Whether a plain delta only grows the last cell of the open table's last
    /// row, so the table's earlier rows keep the spans the last parse gave them.
    fn extend_cell_ok(&self, at: usize) -> bool {
        if self.tail_literal {
            return false;
        }
        let Some(Block {
            body: Body::Table(table),
            ..
        }) = self.open.last()
        else {
            return false;
        };
        let Some(piece) = table
            .rows
            .last()
            .and_then(|row| row.last())
            .and_then(|cell| cell.pieces.last())
        else {
            return false;
        };
        self.tail_span(piece, at)
    }

    /// Grow the last span of the open table's last cell over the new text.
    fn extend_cell(&mut self) {
        let end = self.text.len();
        let Some(piece) = self
            .open
            .last_mut()
            .and_then(|block| match &mut block.body {
                Body::Table(table) => table
                    .rows
                    .last_mut()
                    .and_then(|row| row.last_mut())
                    .and_then(|cell| cell.pieces.last_mut()),
                Body::Flow(_) => None,
            })
        else {
            return;
        };
        piece.end = end as u32;
    }

    /// Append a delta to the last line of an open top-level fenced code block
    /// without a parse. A block of code is the longest thing a model streams and
    /// has no inline construct, so the delta is the whole of its work. Returns
    /// whether the delta was taken.
    fn extend_fence(&mut self, before: usize, delta: &str) -> bool {
        if self.open.len() != 1 {
            return false;
        }
        let text = self.text.as_str();
        let Some(block) = self.open.last_mut() else {
            return false;
        };
        let Some(fence) = block.fence else {
            return false;
        };
        if block.list {
            return false;
        }
        let Some(line) = block.last_line() else {
            return false;
        };
        if line.kind != Kind::Code || line.quote != 0 {
            return false;
        }
        let (quote, indent) = (line.quote, line.indent);
        let first = line.pieces.first().map(Piece::start);
        let last = line
            .pieces
            .last()
            .map(|piece| (piece.source(), piece.end()));
        let Body::Flow(lines) = &mut block.body else {
            return false;
        };
        // A line that is spaces and three or more fence characters closes the
        // block, which only a parse tells apart from code. Only the delta can
        // complete such a line, so a delta without the fence character costs no
        // scan and a backtick inside a line of code costs one line, not a parse.
        if delta.as_bytes().contains(&fence) {
            let start = first.unwrap_or(before);
            if closes_fence(&text[start..], fence) {
                return false;
            }
        }
        // The last span must end at the new text, so the delta continues it.
        if let Some((source, end)) = last
            && (source != Source::Text || end != before)
        {
            return false;
        }
        // A newline that ends a line which holds no code may be the end of a
        // closing fence, which only a parse tells apart from a blank line under
        // the code.
        if last.is_none() && delta.bytes().all(|byte| byte == b'\n' || byte == b'\r') {
            return false;
        }
        let style = self.styles.intern(self.roles.style(CODE, 0, false));
        let mut index = lines.len() - 1;
        let mut cursor = before;
        for part in delta.split_inclusive('\n') {
            let newline = usize::from(part.ends_with('\n'));
            let end = cursor + part.len() - newline;
            // A CRLF line end is not part of the code line.
            let end = match newline {
                1 if end > cursor && text.as_bytes()[end - 1] == b'\r' => end - 1,
                _ => end,
            };
            if end > cursor {
                let line = &mut lines[index];
                match line.pieces.last_mut() {
                    Some(last) if last.source() == Source::Text && last.end() == cursor => {
                        last.end = end as u32;
                    }
                    Some(_) => return false,
                    None => line
                        .pieces
                        .push(Piece::new(Source::Text, cursor, end, style)),
                }
            }
            cursor += part.len();
            if newline == 1 {
                // A `\r` of a CRLF end that came in an earlier delta ends the
                // line here, and is not part of it either.
                trim_cr(text, &mut lines[index]);
                lines.push(LLine {
                    quote,
                    indent,
                    kind: Kind::Code,
                    ..LLine::default()
                });
                index += 1;
            }
        }
        block.end = self.text.len();
        true
    }
}

/// Whether the text after the last layout stops on a fence: a line of optional
/// spaces and three or more `fence` characters with only spaces after them, or
/// the start of one, since the next delta can finish it.
fn closes_fence(text: &str, fence: u8) -> bool {
    text.split('\n').any(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rest = line.trim_start_matches(' ');
        let run = rest.bytes().take_while(|byte| *byte == fence).count();
        run >= 3 && rest.as_bytes()[run..].iter().all(|byte| *byte == b' ')
    })
}

/// Drop the `\r` a CRLF line end left at the end of a code line.
fn trim_cr(text: &str, line: &mut LLine) {
    if let Some(last) = line.pieces.last_mut()
        && last.end > last.start
        && text.as_bytes()[last.end as usize - 1] == b'\r'
    {
        last.end -= 1;
    }
    if line
        .pieces
        .last()
        .is_some_and(|piece| piece.start == piece.end)
    {
        line.pieces.pop();
    }
}

/// Whether a delta can only extend text: letters, digits, spaces and non-ASCII
/// bytes, so no block or inline construct can open or close inside it.
fn plain(delta: &str) -> bool {
    delta
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b' ' || byte >= 0x80)
}

/// Whether the last block is a table row whose line is not complete yet, which
/// the parser splits off as a paragraph and joins to the table again once the
/// line has its cells.
fn partial_row(text: &str, block: &Block) -> bool {
    let Body::Flow(lines) = &block.body else {
        return false;
    };
    lines.first().is_some_and(|line| line.kind == Kind::Text)
        && text[block.start..].trim_start_matches(' ').starts_with('|')
}

/// A one-line paragraph that a delimiter row may still turn into a table.
fn pending_table(text: &str, block: &Block) -> bool {
    let Body::Flow(lines) = &block.body else {
        return false;
    };
    if lines.len() != 1 || lines[0].kind != Kind::Text {
        return false;
    }
    text[block.start..].trim_start_matches(' ').starts_with('|')
}

/// A closing fence the text has started but not finished, which must not show
/// as code.
fn partial_fence(text: &str, block: &Block) -> bool {
    let Some(fence) = block.fence else {
        return false;
    };
    if block.end != text.len() || text.ends_with('\n') {
        return false;
    }
    let line = text.rsplit('\n').next().unwrap_or_default().trim_end();
    !line.is_empty() && line.len() < 3 && line.as_bytes().iter().all(|byte| *byte == fence)
}

/// The byte the open block may show: a trailing run of markup characters stays
/// hidden, since the next delta can consume it.
fn visible_end(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'*' | b'_' | b'~' | b'`' | b'[' | b'(') {
        end -= 1;
    }
    end
}

/// A marker left open at the end of the text.
#[derive(Clone, Copy, Debug)]
enum Opener {
    /// A `*` or `_` run: its character and how many of it are unmatched.
    Emphasis(u8, usize),
    /// A `~~` run.
    Strike,
    /// An inline code span opened by a run of this many backticks.
    Code(usize),
    /// A single `~` that needs one more to close a `~~` run.
    Tilde,
    /// A link label: the closer is `]()`.
    Label,
    /// A link label closed with nothing after it: the closer is `()`.
    Bare,
    /// A link whose `(` destination has streamed but not its `)`.
    Dest,
}

/// The closers that finish the markers `text` leaves open, so a half-written
/// line draws its text without the markers the finished line hides.
fn close_markers(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut stack: Vec<Opener> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        match stack.last() {
            // Inside a code span nothing else is a marker, and only a run of the
            // same length closes it.
            Some(&Opener::Code(len)) => {
                if byte == b'`' {
                    let run = run_len(bytes, i, b'`');
                    if run == len {
                        stack.pop();
                    }
                    i += run;
                } else {
                    i += 1;
                }
                continue;
            }
            // Inside a link destination only its `)` matters.
            Some(&Opener::Dest) => {
                if byte == b')' {
                    stack.pop();
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        match byte {
            b'\\' => i += 2,
            b'`' => {
                let run = run_len(bytes, i, b'`');
                stack.push(Opener::Code(run));
                i += run;
            }
            b'~' => {
                let run = run_len(bytes, i, b'~');
                if run >= 2 {
                    if matches!(stack.last(), Some(Opener::Strike)) {
                        stack.pop();
                    } else {
                        stack.push(Opener::Strike);
                    }
                } else if matches!(stack.last(), Some(Opener::Strike)) {
                    // A single `~` is half of the closing run: the text's own
                    // tilde plus an appended one close the strike.
                    stack.pop();
                    stack.push(Opener::Tilde);
                }
                i += run;
            }
            b'*' | b'_' => {
                let run = run_len(bytes, i, byte);
                let prev = text[..i].chars().next_back();
                let next = text[i + run..].chars().next();
                let word = byte == b'_';
                let opens = next.is_some_and(|c| !c.is_whitespace())
                    && !(word && prev.is_some_and(char::is_alphanumeric));
                let closes = prev.is_some_and(|c| !c.is_whitespace())
                    && !(word && next.is_some_and(char::is_alphanumeric));
                match stack.last_mut() {
                    Some(Opener::Emphasis(opened, left)) if *opened == byte && closes => {
                        if *left > run {
                            *left -= run;
                        } else {
                            stack.pop();
                        }
                    }
                    _ if opens => stack.push(Opener::Emphasis(byte, run)),
                    _ => {}
                }
                i += run;
            }
            b'[' => {
                // A `[` after a word is an index or an array, not the start of a
                // link often enough to leave it alone.
                let word = text[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric);
                if !word && !matches!(stack.last(), Some(Opener::Label)) {
                    stack.push(Opener::Label);
                }
                i += 1;
            }
            b']' => {
                if !matches!(stack.last(), Some(Opener::Label)) {
                    i += 1;
                    continue;
                }
                match bytes.get(i + 1) {
                    // `]` with nothing after it: the label may still get a
                    // destination, so close it with an empty one.
                    None => {
                        *stack.last_mut().expect("a label is open") = Opener::Bare;
                        i += 1;
                    }
                    Some(b'(') => {
                        *stack.last_mut().expect("a label is open") = Opener::Dest;
                        i += 2;
                    }
                    Some(_) => {
                        stack.pop();
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    let mut out = String::new();
    for opener in stack.iter().rev() {
        match *opener {
            Opener::Emphasis(byte, count) => {
                for _ in 0..count {
                    out.push(byte as char);
                }
            }
            Opener::Strike => out.push_str("~~"),
            Opener::Tilde => out.push('~'),
            Opener::Code(count) => {
                for _ in 0..count {
                    out.push('`');
                }
            }
            Opener::Label => out.push_str("]()"),
            Opener::Dest => out.push(')'),
            Opener::Bare => out.push_str("()"),
        }
    }
    out
}

/// The length of the run of `byte` that starts at `at`.
fn run_len(bytes: &[u8], at: usize, byte: u8) -> usize {
    bytes[at..].iter().take_while(|b| **b == byte).count()
}
