//! The transcript: the rows of the ring, the live entries, and the commit of
//! the finished lines.
//!
//! Inline mode prints a finished line once, above the live area, so it stays in
//! the terminal's scrollback; full-screen mode pushes it into the transcript
//! ring and the next frame draws the rows the viewport shows. Only the entries
//! that still grow — a reply that streams, a tool call that waits for its
//! result — stay live, and they are drawn here row by row at the width the
//! sidebar leaves.

use std::sync::Arc;

use crate::app::fold::Folds;
use crate::app::{App, Compacted, DIM, Group, Item, Member, Thinking, Tool};
use crate::theme::{Emphasis, Role};

use self::rows::{Block, Entry, Span, SpanRole};
pub(crate) use self::rows::{LineSink, Sink};

/// The fold state a committed line of the scrollback is drawn with: a line
/// inline mode printed can never change, so its block commits folded.
const COMMITTED_FOLDED: Folds = Folds::new();

/// The column a picture's first cell sits at in its row.
const PICTURE_LEFT: u16 = 2;

impl App {
    /// Prints the lines the finished entries added, and the lines the open reply
    /// has finished, into the scrollback or the ring.
    ///
    /// Every finished item goes through the block rows once, here, so a frame
    /// draws only the rows of the live area. A reply that still streams keeps
    /// its own lines. A block of full screen hands the tool call, the group or
    /// the thought it was drawn from to the ring, so a fold key can draw its
    /// lines again: the item moves into the ring's handle, and no text or diff
    /// of it is copied.
    pub(crate) fn commit(&mut self) {
        if self.pending.is_empty() && !matches!(self.live.last(), Some(Item::Reply { .. })) {
            return;
        }
        let width = self.text_width();
        let margin = self.margin();
        let full = self.screen.is_full();
        let mut lines = std::mem::take(&mut self.block_lines);
        lines.clear(width, margin, full);
        let mut items = std::mem::take(&mut self.pending);
        let mut blocks = std::mem::take(&mut self.finished);
        blocks.clear();
        let mut blank = self.blank;
        for mut item in items.drain(..) {
            let before = lines.line_count();
            let air = blank;
            blank = self.push_lines(&mut item, &mut lines, full, blank);
            let after = lines.line_count();
            let fold = item.fold_id();
            if full
                && fold != 0
                && after > before
                && let Some(frozen) = Frozen::new(item, air)
            {
                let open = self.block_open(fold, &frozen);
                blocks.push(crate::screen::CommitBlock {
                    fold,
                    lines: before..after,
                    open,
                    source: Arc::new(frozen),
                });
            }
        }
        blank = self.push_live_reply(&mut lines, full, blank);
        self.blank = blank;
        let runs = lines.runs();
        if !runs.spans.is_empty() {
            self.screen.commit_blocks(&runs.runs, &runs.spans, &blocks);
        }
        drop(runs);
        blocks.clear();
        self.finished = blocks;
        lines.clear(width, margin, full);
        self.block_lines = lines;
        // The lines the commits hold of the items are the transcript's memory.
        items.clear();
        self.pending = items;
    }

    /// Prints the lines the open reply has finished, behind the entries that
    /// were already waiting.
    fn push_live_reply(&mut self, lines: &mut LineSink, full: bool, blank: bool) -> bool {
        let mut live = std::mem::take(&mut self.live);
        let mut blank = blank;
        if let Some(last) = live.last_mut()
            && matches!(last, Item::Reply { .. })
        {
            blank = self.push_lines(last, lines, full, blank);
        }
        self.live = live;
        blank
    }

    /// A block the fold keys opened or folded draws its rows in the live area,
    /// which the next frame lays out again.
    pub(crate) fn refold(&mut self) {
        self.touch();
    }

    /// Draws the finished block `fold` again with the rows it draws now, whether
    /// its fold state changed or not: an edit a newer one took the diff from
    /// loses those rows, and a call opened inside an open run takes its own.
    /// Answers whether the ring still held it.
    pub(crate) fn refresh(&mut self, fold: u64) -> bool {
        let Some(source) = self.screen.transcript().source(fold) else {
            return false;
        };
        let Ok(frozen) = source.downcast::<Frozen>() else {
            return false;
        };
        self.redraw(fold, frozen)
    }

    /// Marks the edit `call`, in the block `block`, as the one that shows its
    /// diff. A newer edit takes those rows from the block before it: one the ring
    /// holds draws again without them now, and one still live or waiting draws
    /// without them on the next frame.
    pub(crate) fn newest_diff(&mut self, call: u64, block: u64) {
        let old = self.folds.show_diff(call);
        let last = std::mem::replace(&mut self.diff_block, block);
        if old == call {
            return;
        }
        if last != 0 {
            self.refresh(last);
        }
    }

    /// Toggles the finished block Tab acts on and draws its new lines in the
    /// ring: an open run passes the key to the newest of its calls. Answers
    /// whether the key had a finished block to act on; a block that already left
    /// the ring has none.
    pub(crate) fn toggle_finished(&mut self) -> bool {
        if !self.screen.is_full() {
            return false;
        }
        let Some(fold) = self.finished_target() else {
            return false;
        };
        if self.screen.transcript().source(fold).is_none() {
            return false;
        }
        // An open run gives the key to the newest of its calls, which opens and
        // folds one row of the run rather than the run itself.
        if let Some(member) = self.newest_member(fold) {
            self.folds.toggle(member);
            self.refresh(fold);
            return true;
        }
        let all = self.folds.all();
        self.folds.toggle(fold);
        if all {
            // One block folds and the rest stay open: every block the ring
            // holds is drawn again.
            self.reopen_all();
        } else {
            self.reopen(fold);
        }
        true
    }

    /// The finished block Tab acts on: the block at the top of the view while
    /// the user scrolled, else the newest one while the live area holds no
    /// block the key can open.
    fn finished_target(&self) -> Option<u64> {
        let transcript = self.screen.transcript();
        if !transcript.follow()
            && let Some(fold) = transcript.block_at(transcript.top())
        {
            return Some(fold);
        }
        if self.live.iter().any(|item| item.fold_id() != 0) {
            return None;
        }
        transcript.newest_block()
    }

    /// Draws every finished block the ring holds again with the fold state it
    /// has now: Ctrl+O opens them all, and a Tab after it folds one of them.
    pub(crate) fn reopen_all(&mut self) {
        if !self.screen.is_full() {
            return;
        }
        for index in 0..self.screen.transcript().blocks() {
            if let Some(fold) = self.screen.transcript().block_fold(index) {
                self.reopen(fold);
            }
        }
        self.touch();
    }

    /// Draws the finished block `fold` again with the fold state it has now and
    /// replaces its lines in the ring. Answers whether the ring still held it.
    pub(crate) fn reopen(&mut self, fold: u64) -> bool {
        let Some(source) = self.screen.transcript().source(fold) else {
            return false;
        };
        let Ok(frozen) = source.downcast::<Frozen>() else {
            return false;
        };
        if self.screen.transcript().rendered_open(fold) == Some(self.block_open(fold, &frozen)) {
            return false;
        }
        self.redraw(fold, frozen)
    }

    /// Takes the group of the committed block `fold` back out of the ring, hands
    /// it to `edit`, and draws the block again in its place. Answers whether the
    /// ring still held it.
    pub(crate) fn edit_block(&mut self, fold: u64, edit: impl FnOnce(&mut Group)) -> bool {
        let Some(source) = self.screen.take_source(fold) else {
            return false;
        };
        let mut frozen = match source.downcast::<Frozen>() {
            Ok(frozen) => frozen,
            Err(source) => {
                self.screen.set_source(fold, source);
                return false;
            }
        };
        match Arc::get_mut(&mut frozen).map(|frozen| &mut frozen.kind) {
            Some(FrozenKind::Group(group)) => edit(group),
            _ => {
                self.screen.set_source(fold, frozen);
                return false;
            }
        }
        self.redraw(fold, frozen)
    }

    /// Gives the committed compaction row `fold` the checkpoint text the journal
    /// recorded and draws the row again in the ring. Answers whether the ring
    /// still held it.
    pub(crate) fn fill_checkpoint(&mut self, fold: u64, text: Arc<str>) -> bool {
        let Some(source) = self.screen.take_source(fold) else {
            return false;
        };
        let mut frozen = match source.downcast::<Frozen>() {
            Ok(frozen) => frozen,
            Err(source) => {
                self.screen.set_source(fold, source);
                return false;
            }
        };
        match Arc::get_mut(&mut frozen).map(|frozen| &mut frozen.kind) {
            Some(FrozenKind::Compacted(row)) => row.text = Some(text),
            _ => {
                self.screen.set_source(fold, frozen);
                return false;
            }
        }
        self.redraw(fold, frozen)
    }

    /// Draws the committed block `fold` again from `frozen` and gives the ring
    /// its new lines, in the room of the old ones. Answers whether the ring still
    /// held the block.
    fn redraw(&mut self, fold: u64, frozen: Arc<Frozen>) -> bool {
        let open = self.block_open(fold, &frozen);
        let width = self.text_width();
        let mut lines = std::mem::take(&mut self.block_lines);
        lines.clear(width, self.margin(), true);
        let mut spans = scratch();
        let block = frozen.block(&mut spans);
        let styles = self.entry_styles(&block);
        let mut rows = rows_for(0, u16::MAX, 0, self.transcript_width());
        rows.blank = frozen.blank;
        rows::paint_entry(
            &mut rows,
            &mut lines,
            &Entry { id: fold, block },
            &self.folds,
            &styles,
        );
        lines.end();
        let runs = lines.runs();
        let replaced = self
            .screen
            .replace_block(fold, open, &runs.runs, &runs.spans);
        drop(runs);
        // The block draws from the source it was given again, whether or not the
        // ring had the room to take its new lines.
        self.screen.set_source(fold, frozen);
        lines.clear(width, self.margin(), true);
        self.block_lines = lines;
        self.touch();
        replaced
    }

    /// Whether the block drawn from `frozen` is open: a run that still goes draws
    /// one row per call, and every other block follows the fold state.
    fn block_open(&self, fold: u64, frozen: &Frozen) -> bool {
        match &frozen.kind {
            FrozenKind::Group(group) if group.going => true,
            _ => self.folds.is_open(fold),
        }
    }

    /// Renders one finished item into the lines of a commit, under the blank row
    /// `blank` says a neighbour drew, and answers whether a blank row ends the
    /// item. Full screen draws its folded rows with the hint that a tab opens
    /// them, since a later key can; the scrollback of inline mode cannot change,
    /// so its blocks commit folded and say only how many lines they left out.
    fn push_lines(
        &mut self,
        item: &mut Item,
        sink: &mut LineSink,
        full: bool,
        blank: bool,
    ) -> bool {
        let width = self.transcript_width();
        let dim = self.screen.intern(DIM);
        let started = sink.line_count();
        match item {
            Item::Reply { md, done } => {
                let finished = md.lines().0.len();
                md.set_width(width);
                // The ring stores the batch in order, so the lines of this reply
                // take the ring lines from here on.
                let base = self.screen.transcript().len();
                let mut held: Option<crate::markdown::Origin> = None;
                let mut from = 0;
                for index in *done..finished {
                    if index == *done {
                        from = sink.line_count();
                    }
                    let line = &md.lines().0[index];
                    let start = sink.at();
                    sink.margin_start();
                    for piece in line.pieces() {
                        let style = self.screen.intern(md.styles().get(piece.style()));
                        sink.run(md.piece_text(piece), style);
                    }
                    sink.line_from(start);
                    let origin = md.origin(index);
                    if origin != held {
                        let last = sink.line_count() - 1;
                        if let Some(origin) = held.take() {
                            self.keep_source(md, &origin, base + from as u64, last - from);
                        }
                        from = last;
                        held = origin;
                    }
                }
                if let Some(origin) = held {
                    let last = sink.line_count();
                    self.keep_source(md, &origin, base + from as u64, last - from);
                }
                *done = finished;
            }
            Item::Plain { lines, dim: faded } => {
                let style = if *faded { dim } else { 0 };
                for line in lines.iter() {
                    sink.text_line(line, style);
                }
            }
            Item::Answer { line, ok } => {
                let role = if *ok { Role::Add } else { Role::Error };
                let style = self.style(role, Emphasis::Plain);
                sink.text_line(line, style);
            }
            Item::Picture(picture) => {
                // A terminal that draws placeholders gets the image once and the
                // rows of cells that name it; another gets the half-block cells.
                let placed = self.placeholder_cells(picture);
                let (cells, columns, height) = match placed {
                    Some((_, columns, height)) => (Vec::new(), columns, height),
                    None => self.picture_cells(picture),
                };
                let base = self.screen.transcript().len();
                let block = Block::Picture {
                    cells: &cells,
                    placeholder: placed.map(|(style, _, _)| style),
                    columns,
                    height,
                    chip: &picture.chip,
                };
                let styles = self.entry_styles(&block);
                let mut drawn = rows_for(0, u16::MAX, 0, width);
                drawn.blank = blank;
                // A picture is finished when it is pushed, so it commits folded
                // whatever the fold state says, as a line of the scrollback does.
                rows::paint_entry(
                    &mut drawn,
                    sink,
                    &Entry { id: 0, block },
                    &COMMITTED_FOLDED,
                    &styles,
                );
                sink.end();
                // The cells of a picture answer a copy with nothing, and the chip
                // answers with itself: the glyphs of half blocks are no text. The
                // chip is the item's last line and the cells the lines above it; a
                // blank row may come before them.
                let chip = base.saturating_add(sink.line_count().saturating_sub(1) as u64);
                let first = chip.saturating_sub(u64::from(height));
                if full
                    && height > 0
                    && placed.is_none()
                    && let Some(source) = picture.source.clone()
                {
                    self.screen.add_picture(
                        first,
                        height,
                        columns,
                        sink.margin().saturating_add(PICTURE_LEFT),
                        source,
                    );
                }
                for row in 0..u64::from(height) {
                    self.screen
                        .set_origin(first.saturating_add(row), 1, "", true, true);
                }
                self.screen.set_origin(chip, 1, &picture.chip, false, false);
                return drawn.blank;
            }
            _ => {
                let mut spans = scratch();
                let Some(block) = block_of(item, &mut spans) else {
                    return blank;
                };
                let styles = self.entry_styles(&block);
                let mut rows = rows_for(0, u16::MAX, 0, width);
                rows.blank = blank;
                // A line in the scrollback cannot change: inline mode commits
                // its blocks folded, whatever the fold state says.
                let folds = if full { &self.folds } else { &COMMITTED_FOLDED };
                rows::paint_entry(
                    &mut rows,
                    sink,
                    &Entry {
                        id: item.fold_id(),
                        block,
                    },
                    folds,
                    &styles,
                );
                sink.end();
                return rows.blank;
            }
        }
        // A line of text or of a note under the item takes the blank row away.
        if sink.line_count() > started {
            false
        } else {
            blank
        }
    }

    /// Give the ring the text of the block that took the batch lines from `line`
    /// on, as the reply wrote it, so a copy of their rows reads it back.
    fn keep_source(
        &mut self,
        md: &crate::markdown::Markdown,
        origin: &crate::markdown::Origin,
        line: u64,
        lines: usize,
    ) {
        let text = md.text().get(origin.bytes.clone()).unwrap_or_default();
        self.screen
            .set_origin(line, lines as u64, text, origin.code, origin.label);
    }

    /// The rows the live entries need at the width of the screen.
    pub(crate) fn live_rows(&mut self) -> u16 {
        let mut blank = self.blank;
        let mut live = std::mem::take(&mut self.live);
        let mut total: u16 = 0;
        for item in &mut live {
            let (rows, after) = self.item_rows(item, blank);
            total = total.saturating_add(rows);
            blank = after;
        }
        self.live = live;
        total
    }

    /// Draws the live entries from row `from` down, its first rows left out when
    /// they need more than `limit`, and answers the row after them. The rows it
    /// leaves out stay in [`App::live_skip`], so a click counts from the rows the
    /// frame drew.
    pub(crate) fn paint_live(&mut self, from: u16, limit: u16, _dim: u32) -> u16 {
        let width = self.transcript_width();
        let total = self.live_rows();
        let skip = total.saturating_sub(limit);
        self.live_skip = skip;
        let mut left = skip;
        let mut y = from;
        let mut blank = self.blank;
        let mut live = std::mem::take(&mut self.live);
        for item in &mut live {
            let (rows, after) = self.item_rows(item, blank);
            if left >= rows {
                left -= rows;
                blank = after;
                continue;
            }
            y += self.paint_item(item, y, left, width, blank).0;
            left = 0;
            blank = after;
        }
        self.live = live;
        y
    }

    /// The rows one live entry takes, drawn under the blank row `blank` says a
    /// neighbour drew, and whether a blank row ends it.
    fn item_rows(&mut self, item: &mut Item, blank: bool) -> (u16, bool) {
        let width = self.transcript_width();
        match item {
            Item::Reply { md, .. } => {
                md.set_width(width);
                let rows = md.lines().1.len() as u16;
                (rows, rows == 0 && blank)
            }
            Item::Plain { lines, .. } => {
                let rows = lines.len() as u16;
                (rows, rows == 0 && blank)
            }
            Item::Answer { .. } => (1, false),
            _ => {
                // The block's own number, so the count opens what the frame
                // draws: an open block counts its open rows.
                let id = item.fold_id();
                let mut spans = scratch();
                let Some(block) = block_of(item, &mut spans) else {
                    return (0, blank);
                };
                let styles = self.entry_styles(&block);
                let mut rows = rows_for(0, 0, 0, width);
                rows.count = true;
                rows.blank = blank;
                rows::paint_entry(
                    &mut rows,
                    &mut Null,
                    &Entry { id, block },
                    &self.folds,
                    &styles,
                );
                (rows.drawn, rows.blank)
            }
        }
    }

    /// Draws one live entry from row `y`, its first `skip` rows left out, under
    /// the blank row `blank` says a neighbour drew, and answers how many rows it
    /// drew and whether a blank row ends it.
    fn paint_item(
        &mut self,
        item: &mut Item,
        y: u16,
        skip: u16,
        width: u16,
        blank: bool,
    ) -> (u16, bool) {
        let margin = self.margin();
        let full = self.text_width();
        let right = margin.saturating_add(width);
        match item {
            Item::Reply { md, .. } => {
                md.set_width(width);
                let live = md.lines().1;
                let rows = live.len() as u16;
                for (offset, line) in live.iter().enumerate().skip(usize::from(skip)) {
                    let mut x = margin;
                    let at = y + offset as u16 - skip;
                    for piece in line.pieces() {
                        let style = self.screen.intern(md.styles().get(piece.style()));
                        x = self
                            .screen
                            .put_str_until(x, at, md.piece_text(piece), style, right);
                    }
                }
                (rows - skip, false)
            }
            Item::Plain { lines, dim } => {
                let style = if *dim { self.screen.intern(DIM) } else { 0 };
                for (offset, line) in lines.iter().enumerate().skip(usize::from(skip)) {
                    self.screen
                        .put_str_until(margin, y + offset as u16 - skip, line, style, right);
                }
                (lines.len() as u16 - skip, false)
            }
            // The one line of an answer, in the role the answer gave it: the add
            // role of an allowed call, the error role of a refused one.
            Item::Answer { line, ok } => {
                if skip > 0 {
                    return (0, false);
                }
                let role = if *ok { Role::Add } else { Role::Error };
                let style = self.style(role, Emphasis::Plain);
                self.screen.put_str_until(margin, y, line, style, right);
                (1, false)
            }
            _ => {
                let mut spans = scratch();
                let Some(block) = block_of(item, &mut spans) else {
                    return (0, blank);
                };
                let styles = self.entry_styles(&block);
                let mut rows = rows_for(y, y.saturating_add(1), skip, width);
                rows.bottom = view_bottom(y);
                rows.blank = blank;
                let mut sink = rows::GridSink {
                    screen: &mut self.screen,
                    width: full,
                    margin,
                };
                rows::paint_entry(
                    &mut rows,
                    &mut sink,
                    &Entry {
                        id: item.fold_id(),
                        block,
                    },
                    &self.folds,
                    &styles,
                );
                (rows.drawn, rows.blank)
            }
        }
    }
}

/// The bottom of the rows a live entry may draw in: the rest of the screen.
fn view_bottom(y: u16) -> u16 {
    y.saturating_add(u16::MAX / 2)
}

/// The rows of one block, from `y`, `limit` rows long, its first `skip` rows
/// left out.
fn rows_for(y: u16, limit: u16, skip: u16, width: u16) -> rows::Rows {
    rows::Rows {
        y,
        bottom: y.saturating_add(limit),
        skip,
        width,
        drawn: 0,
        count: false,
        blank: false,
    }
}

/// The block one item draws, a tool's row spans in the caller's `spans`.
///
/// The item's own text stays where it is: the block borrows it, and a number the
/// rows print goes into a buffer on the stack, so drawing a block copies nothing.
fn block_of<'a>(item: &'a Item, spans: &'a mut [Span<'a>; SPANS]) -> Option<Block<'a>> {
    Some(match item {
        Item::User { text, .. } => Block::User { text },
        Item::Thought(think) => Block::Thinking {
            seconds: think.seconds,
            text: &think.text,
        },
        Item::Thinking(text) => Block::Thinking { seconds: 0, text },
        Item::Tool(tool) => Block::Tool(tool.row(spans)),
        Item::Group(group) => Block::Group(group.row()),
        Item::Compacted(row) => Block::Compacted(row.row()),
        Item::Rule(usage) => Block::Rule(*usage),
        Item::Steer { text, queued } => Block::Steer {
            text,
            queued: *queued,
        },
        Item::Reply { .. } | Item::Plain { .. } | Item::Answer { .. } => return None,
        Item::Picture(_) => return None,
    })
}

/// What a committed block of full screen draws from again: the tool call, the
/// group, the thought or the compaction its row was drawn from, moved out of
/// the item when the item was committed. The item's text and diff move with it,
/// so the ring holds the source of a block that can open and no copy of it.
#[derive(Debug)]
pub(crate) struct Frozen {
    /// Whether a blank row was drawn above the block already: it draws its rows
    /// again in the ring, beside the neighbour it had.
    blank: bool,
    /// The item the block draws its rows from.
    kind: FrozenKind,
}

/// The item a committed block draws again.
#[derive(Debug)]
enum FrozenKind {
    /// One finished tool call.
    Tool(Box<Tool>),
    /// A run of tool calls, in one row.
    Group(Box<Group>),
    /// The reasoning of one turn.
    Thought(Box<Thinking>),
    /// One compaction of the history.
    Compacted(Box<Compacted>),
}

impl Frozen {
    /// The source of `item`, for an item a fold key can draw again, drawn under
    /// the blank row `blank` says a neighbour drew.
    fn new(item: Item, blank: bool) -> Option<Frozen> {
        let kind = match item {
            Item::Tool(tool) => FrozenKind::Tool(tool),
            Item::Group(group) => FrozenKind::Group(group),
            Item::Thought(think) => FrozenKind::Thought(think),
            Item::Compacted(row) => FrozenKind::Compacted(row),
            _ => return None,
        };
        Some(Frozen { blank, kind })
    }

    /// The run of calls the block draws, when it draws one: a click looks for
    /// the member's own row inside it.
    pub(crate) fn group(&self) -> Option<&Group> {
        match &self.kind {
            FrozenKind::Group(group) => Some(group),
            _ => None,
        }
    }

    /// The block the item draws, in the caller's spans.
    fn block<'a>(&'a self, spans: &'a mut [Span<'a>; SPANS]) -> Block<'a> {
        match &self.kind {
            FrozenKind::Tool(tool) => Block::Tool(tool.row(spans)),
            FrozenKind::Group(group) => Block::Group(group.row()),
            FrozenKind::Thought(think) => Block::Thinking {
                seconds: think.seconds,
                text: &think.text,
            },
            FrozenKind::Compacted(row) => Block::Compacted(row.row()),
        }
    }
}

/// Where a member of a run draws its rows.
#[derive(Clone, Copy)]
struct Place {
    /// True when the fold of the run is open.
    open: bool,
    /// The tree line the member hangs from, `None` for the members of a run
    /// that draws no tree.
    hang: Option<rows::Hang>,
}

/// The members of a run, drawn in their order: a call as its own row, and a
/// thought as its reasoning rows. Each row is built from the member in place, as
/// a lone call's row is, so opening a run copies and allocates nothing, however
/// many members it holds.
impl Group {
    /// Draws the member `member`, and answers the number of the call whose rows
    /// start there. A member that draws under the run's tree line hangs from it:
    /// its rows shift in by the line, and the rows' own width with them.
    fn paint_member(
        &self,
        member: &Member,
        rows: &mut rows::Rows,
        sink: &mut dyn Sink,
        styles: &rows::Styles,
        folds: &Folds,
        place: Place,
    ) -> Option<u64> {
        let Some(hang) = place.hang else {
            return self.draw_member(member, rows, sink, styles, folds, place.open);
        };
        let width = rows.width;
        rows.width = width.saturating_sub(rows::INDENT);
        let mut branch = rows::Branch::new(sink, hang);
        let fold = self.draw_member(member, rows, &mut branch, styles, folds, place.open);
        branch.settle();
        rows.width = width;
        fold
    }

    /// Draws the member `member` in the columns the sink gives it, and answers
    /// the number of the call whose rows start there.
    fn draw_member(
        &self,
        member: &Member,
        rows: &mut rows::Rows,
        sink: &mut dyn Sink,
        styles: &rows::Styles,
        folds: &Folds,
        open: bool,
    ) -> Option<u64> {
        let mut spans = scratch();
        match member {
            Member::Call(tool) => {
                // A run of one call keeps that call's own row, and the run's
                // state with it; every other call opens, and folds, by itself.
                let (member_open, shown) = if self.calls == 1 {
                    (open, folds.diff_shown(tool.fold, tool.diff.len(), open))
                } else {
                    let member_open = folds.is_open(tool.fold);
                    (
                        member_open,
                        folds.diff_shown(tool.fold, tool.diff.len(), member_open),
                    )
                };
                let drawn = rows::member_styles(styles, &tool.name, tool.state);
                rows::paint_tool(
                    rows,
                    sink,
                    &tool.row(&mut spans),
                    member_open,
                    shown,
                    &drawn,
                );
                Some(tool.fold)
            }
            Member::Thought(think) => {
                rows::paint_thinking(rows, sink, think.seconds, &think.text, open, styles);
                None
            }
        }
    }

    /// How the member at `index` hangs from the tree line of the run while it
    /// draws open, `None` for a run that draws no tree: one that still goes, a
    /// run of one call, and a folded run.
    fn hang(&self, index: usize, open: bool, styles: &rows::Styles) -> Option<rows::Hang> {
        if !open || self.going || self.calls < 2 {
            return None;
        }
        Some(rows::Hang::new(styles, index + 1 == self.members.len()))
    }
}

impl Group {
    /// The newest call of the run, whose row Tab opens while the run is open.
    fn newest_call(&self) -> Option<u64> {
        self.members.iter().rev().find_map(|member| match member {
            Member::Call(tool) => Some(tool.fold),
            Member::Thought(_) => None,
        })
    }
}

impl rows::Members for Group {
    fn paint_members(
        &self,
        rows: &mut rows::Rows,
        sink: &mut dyn Sink,
        styles: &rows::Styles,
        folds: &Folds,
        open: bool,
    ) {
        for (index, member) in self.members.iter().enumerate() {
            let hang = self.hang(index, open, styles);
            let place = Place { open, hang };
            self.paint_member(member, rows, sink, styles, folds, place);
        }
    }

    fn member_at(
        &self,
        rows: &mut rows::Rows,
        styles: &rows::Styles,
        folds: &Folds,
        open: bool,
        offset: u16,
    ) -> Option<u64> {
        let mut null = Null;
        for (index, member) in self.members.iter().enumerate() {
            let start = rows.drawn;
            let hang = self.hang(index, open, styles);
            let place = Place { open, hang };
            let fold = self.paint_member(member, rows, &mut null, styles, folds, place);
            if offset >= start && offset < rows.drawn {
                return fold;
            }
        }
        None
    }

    fn paint_failed(
        &self,
        rows: &mut rows::Rows,
        sink: &mut dyn Sink,
        styles: &rows::Styles,
        folds: &Folds,
    ) {
        for member in &self.members {
            let Member::Call(tool) = member else {
                continue;
            };
            if tool.state != rows::State::Failed {
                continue;
            }
            // Each call takes its own styles where it is painted: a card the
            // tint of its failure, a plain row the run's paints.
            let place = Place {
                open: false,
                hang: None,
            };
            self.paint_member(member, rows, sink, styles, folds, place);
        }
    }
}

/// The spans one tool's row fills on the stack: a block that draws a call
/// allocates nothing, however many frames draw it again.
fn scratch<'a>() -> [Span<'a>; SPANS] {
    [Span {
        text: "",
        role: SpanRole::Dim,
    }; SPANS]
}

/// The most spans one tool row pushes to its right edge.
pub(crate) const SPANS: usize = 3;

/// The sink that draws nothing, for the rows a frame only counts.
struct Null;

impl rows::Sink for Null {
    fn row(&mut self, _y: u16) {}

    fn fill(&mut self, _y: u16, _style: u32) {}

    fn fill_from(&mut self, _y: u16, _x: u16, _width: u16, _style: u32) {}

    fn put(&mut self, _y: u16, x: u16, _text: &str, _style: u32) -> u16 {
        x
    }

    fn put_until(&mut self, _y: u16, x: u16, _text: &str, _style: u32, _limit: u16) -> u16 {
        x
    }

    fn end(&mut self) {}
}

/// The rows of the transcript's blocks: user turns, tool calls, thinking, turn
/// rules, and the steer and new-lines rows.
///
/// Every row draws from borrowed text: a message, a tool result and a diff line
/// stay in the buffer that owns them, and a row writes slices of them into the
/// grid or into the lines of a commit. A frame that draws a block interns one
/// style id per part of it, so no cell builds a style, and the work of a frame
/// is the rows it shows.
///
/// The app keeps one [`Folds`] beside its blocks, numbers each block with
/// `Folds::add`, and draws the entries with `App::paint_blocks`. Tab calls
/// `Folds::toggle_at` with the block under the cursor or `None`, and Ctrl+O
/// calls `Folds::open_all`.
pub(crate) mod rows {
    use std::fmt;
    use std::ops::Range;

    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    use crate::app::OwnedDiff;
    use crate::app::paint::theme::token_role;
    use crate::app::{Group, Item};

    use super::{Frozen, Null, rows_for};
    use crate::app::{App, Counted, Tool};
    use crate::highlight::{self, Token};
    use crate::picture_native;
    use crate::screen::{Color, Run, Style};
    use crate::theme::{Emphasis, Rgb, Role, Theme};

    use crate::app::fold::Folds;

    /// The columns of the main area a diff needs before it draws its old lines
    /// and its new lines side by side.
    const SIDE_BY_SIDE: u16 = 120;

    /// How far the tint of an added or a removed line moves the block's fill
    /// toward the add or the delete role, as the mockup tints its diff lines.
    const DIFF_TINT: u8 = 12;

    /// How far the fill of a half no line of that side reaches moves toward the
    /// line color, as the mockup washes its gap fill.
    const DIFF_GAP: u8 = 4;

    /// The column of a diff row its sign starts at, and the one its code starts
    /// at.
    const SIGN: u16 = 8;

    /// The spaces the fill of a block row writes to the right edge of the row.
    const SPACES: &str = "                                ";

    /// What a folded row says it left out, on a row that a key can open.
    const MORE_HINT: &str = " more lines · tab opens";

    /// What a folded row says it left out, on a line that is printed once.
    const MORE: &str = " more lines";

    /// The most lines of a call's output that an open row draws under it.
    const OUTPUT_ROWS: usize = 12;

    /// The lines of a card's output body it shows while it is folded.
    const BODY_LINES: usize = 10;

    /// The bar a card's rows start with.
    const BAR: &str = "\u{258e}";

    /// The line an open run hangs its members from: the edge of its summary
    /// card, and the lead of the rows before a member's branch.
    const RAIL: &str = "\u{2502}";

    /// The lead of the first row of a member that is not the last of the run.
    const BRANCH: &str = "\u{251c}\u{2500}\u{2500}\u{2500}";

    /// The lead of the first row of the last member of the run.
    const END: &str = "\u{2514}\u{2500}\u{2500}\u{2500}";

    /// The columns a member of an open run keeps for the run's tree line.
    pub(crate) const INDENT: u16 = 4;

    /// The rows a run's summary row takes before its members: its padding row,
    /// its own row and the padding row under it.
    const SUMMARY_ROWS: u16 = 3;

    /// The state of a tool call: its glyph, and the tint of its block.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum State {
        /// The call runs: `◐` in the warning role, and the live duration.
        Running,
        /// The call finished: `✓` in the add role, and its result.
        Done,
        /// The call failed: `✗` in the error role, and an ` error ` chip.
        Failed,
    }

    /// What one diff line did.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum DiffKind {
        /// A line both sides keep.
        #[allow(
            dead_code,
            reason = "a hand-made diff and the tests draw context lines"
        )]
        Context,
        /// A line the edit added.
        Add,
        /// A line the edit removed.
        Delete,
    }

    /// One line of the diff under an edit.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Diff<'a> {
        /// The line number in the file.
        pub(crate) number: u32,
        /// What the line did.
        pub(crate) kind: DiffKind,
        /// The line's text, without its `+` or `-`.
        pub(crate) text: &'a str,
        /// The tokenizer's spans of the text, empty while its language is unknown.
        pub(crate) tokens: &'a [highlight::Span],
    }

    /// One span of a tool row's result, at the right edge of the main area.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Span<'a> {
        /// The text, with the spaces between it and the span before it.
        pub(crate) text: &'a str,
        /// The role it draws in.
        pub(crate) role: SpanRole,
    }

    /// The role of one span of a tool row's result.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum SpanRole {
        /// Something added: `+14`.
        Add,
        /// Something removed: `-3`.
        Delete,
        /// A quiet count or duration.
        Dim,
        /// The tool's own text.
        Text,
    }

    /// One tool call: its row, and the diff under it while it is an edit.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ToolRow<'a> {
        /// The tool's name, drawn bold: `edit`, `bash`.
        pub(crate) name: &'a str,
        /// Its target: the path, the command, the pattern.
        pub(crate) target: &'a str,
        /// What the call did.
        pub(crate) state: State,
        /// The result at the right edge: `+14 -3`, `8 passed`, `no such file`.
        pub(crate) right: &'a [Span<'a>],
        /// The lines of the edit it makes, drawn under its row.
        pub(crate) diff: &'a [OwnedDiff],
        /// The error it ended with, drawn under its row while it is open.
        pub(crate) error: Option<&'a str>,
        /// The tail of the output it printed, drawn under its row while it is
        /// open and is neither an edit nor a failure.
        pub(crate) output: Option<&'a str>,
        /// The lines the cut of that tail took off the output.
        pub(crate) earlier: u64,
    }

    /// A compaction of the history: one dim row, open to the checkpoint it wrote.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct CompactedRow<'a> {
        /// The prompt tokens before and after it, which a journal row leaves out.
        pub(crate) tokens: Option<(u64, u64)>,
        /// How many messages the checkpoint stands in for.
        pub(crate) messages: u32,
        /// The checkpoint text, drawn under the row while it is open.
        pub(crate) text: Option<&'a str>,
    }

    /// A run of tool calls of a turn, grouped into one row until opened.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct GroupRow<'a> {
        /// The tool names with their call counts, in the order the calls came.
        pub(crate) names: &'a [Counted],
        /// How many of the calls failed.
        pub(crate) failed: u32,
        /// Whether the run holds an edit with a diff.
        pub(crate) edits: bool,
        /// The lines its edits added.
        pub(crate) added: u64,
        /// The lines its edits removed.
        pub(crate) removed: u64,
        /// The language every one of those edits shares, when they share one.
        pub(crate) language: Option<&'a str>,
        /// The lines the calls read.
        pub(crate) lines: u64,
        /// The calls the group holds: the summary counts them and no thought.
        pub(crate) calls: u32,
        /// Whether the run still goes, which draws its members and no summary row.
        pub(crate) going: bool,
        /// The calls and thoughts the group holds, drawn one row each while the
        /// group is open.
        pub(crate) members: &'a dyn Members,
        /// The newest call of the group that made a diff, which draws its lines
        /// under the summary row while the group is folded.
        pub(crate) newest: Option<&'a Tool>,
    }

    /// The members of a run, which a frame draws in their order while the run
    /// shows them.
    ///
    /// The app's group paints its own members: each call's row is built from the
    /// call it draws, as a lone call's row is, so an open run allocates nothing
    /// and its rows count through the same rows they draw in.
    pub(crate) trait Members: fmt::Debug {
        /// Paints one row per call and one row per thought, from the rows' next
        /// row down. A call draws as a lone call's row does, its own diff or its
        /// error under it while it is open; the run of one call draws that call's
        /// own row, and the run's `open` state with it.
        fn paint_members(
            &self,
            rows: &mut Rows,
            sink: &mut dyn Sink,
            styles: &Styles,
            folds: &Folds,
            open: bool,
        );

        /// The rows of the cards of the run's failed calls, in their order, from
        /// the rows' next row down: a folded run keeps them under its summary row,
        /// each with its own fold, so a call's error stays on the screen.
        fn paint_failed(
            &self,
            rows: &mut Rows,
            sink: &mut dyn Sink,
            styles: &Styles,
            folds: &Folds,
        );

        /// The number of the call whose rows sit `offset` rows into the open run,
        /// `None` on a thought's rows or under the last member: a click on a
        /// call's own row opens that call.
        fn member_at(
            &self,
            rows: &mut Rows,
            styles: &Styles,
            folds: &Folds,
            open: bool,
            offset: u16,
        ) -> Option<u64>;
    }

    /// What one run used, for the dim rule that ends it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct Usage {
        /// The seconds the run took.
        pub(crate) seconds: u32,
        /// The tokens it used.
        pub(crate) tokens: u64,
        /// What it cost, in cents.
        pub(crate) cents: u32,
    }

    /// One block of the transcript.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Block<'a> {
        /// A user turn: `›` in the accent, the text bold, no fill.
        User {
            /// The message, which wraps with a two-column indent.
            text: &'a str,
        },
        /// The model's reasoning: one dim italic row `thought for 4s  ▸`.
        Thinking {
            /// The seconds it thought for.
            seconds: u32,
            /// The newest line of the reasoning, drawn while the block is open.
            text: &'a str,
        },
        /// One tool call.
        Tool(ToolRow<'a>),
        /// A run of tool calls, in one row until it opens.
        Group(GroupRow<'a>),
        /// The dim rule a turn ends with.
        Rule(Usage),
        /// A compaction: one dim row, and the checkpoint text while it is open.
        Compacted(CompactedRow<'a>),
        /// A message that steers the run, above the text box.
        Steer {
            /// The message.
            text: &'a str,
            /// True for a message that waits for the next turn.
            queued: bool,
        },
        /// The row that says how far the viewport is from the bottom.
        NewLines {
            /// The lines the viewport is behind.
            count: u64,
        },
        /// One image: its half-block cells and the chip under them.
        Picture {
            /// The style id of every cell, row by row, `columns` of them to a row.
            /// Empty when the block draws only its chip or placeholder rows.
            cells: &'a [u32],
            /// The style id of every placeholder row of the picture, `None` when
            /// the rows are the half-block cells.
            placeholder: Option<u32>,
            /// How many cells a row of the picture holds.
            columns: u16,
            /// How many rows of cells it holds.
            height: u16,
            /// The dim row under the cells, `▣ image 64x48 · 3 KB`.
            chip: &'a str,
        },
    }

    /// One entry of the transcript: a block and the number [`Folds`] knows it by.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Entry<'a> {
        /// The block's number, from [`Folds::add`].
        pub(crate) id: u64,
        /// What the block draws.
        pub(crate) block: Block<'a>,
    }

    /// The style ids one block draws its rows with, all over the block's tint.
    #[derive(Clone, Copy, Debug)]
    struct Paints {
        /// The fill of the block, `None` for a block without one.
        fill: Option<u32>,
        /// The state glyph.
        glyph: u32,
        /// The bar in column 0 of a card's rows.
        bar: u32,
        /// A tool's name.
        name: u32,
        /// The block's own text.
        text: u32,
        /// A count, a hint, a line number.
        dim: u32,
        /// Something added.
        add: u32,
        /// Something removed.
        delete: u32,
        /// The accent: the `›` of a turn, the `↳` of a steer row.
        accent: u32,
        /// The ` error ` chip.
        chip: u32,
        /// The error text under a failed call's row.
        error: u32,
    }

    /// Where the next row of a block goes: the row of the grid, the rows a frame
    /// leaves out above its area, and the bottom of that area.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Rows {
        /// The row the next line goes at.
        pub(crate) y: u16,
        /// The row after the area.
        pub(crate) bottom: u16,
        /// How many rows to leave out before drawing.
        pub(crate) skip: u16,
        /// The columns the rows lay out in.
        pub(crate) width: u16,
        /// The rows drawn so far.
        pub(crate) drawn: u16,
        /// Count the rows without drawing them.
        #[allow(dead_code, reason = "the block count of a frame turns rows off")]
        pub(crate) count: bool,
        /// Whether the row before the next one is a blank row.
        pub(crate) blank: bool,
    }

    impl Rows {
        /// The next row, for a row with content on it.
        fn line(&mut self, sink: &mut dyn Sink) -> Option<u16> {
            self.blank = false;
            let y = self.next()?;
            sink.row(y);
            Some(y)
        }

        /// The next row, for a blank row: one that the row before it is not,
        /// since one blank row separates two blocks at most.
        fn air(&mut self, sink: &mut dyn Sink) {
            if self.blank {
                return;
            }
            self.blank = true;
            if let Some(y) = self.next() {
                sink.row(y);
            }
        }

        /// The row the next line goes at, `None` while it sits above the area or
        /// below it. A row this frame leaves out still takes its place.
        fn next(&mut self) -> Option<u16> {
            if self.count {
                self.drawn = self.drawn.saturating_add(1);
                return None;
            }
            if self.skip > 0 {
                self.skip -= 1;
                return None;
            }
            let y = self.y;
            self.y = self.y.saturating_add(1);
            if y >= self.bottom {
                return None;
            }
            self.drawn = self.drawn.saturating_add(1);
            Some(y)
        }
    }

    impl App {
        /// The number of the call whose own row a click on the block `fold` at
        /// `offset` rows into it landed on, while that block is an open run.
        ///
        /// The rows are counted from the blank row above the block, which the
        /// click's row tells apart from the block's own first row. A run of one
        /// call has no member rows: its own row is the block's row.
        pub(crate) fn member_at(&mut self, fold: u64, offset: u16) -> Option<u64> {
            self.with_open_run(fold, |app, group| app.member_of(group, offset))
                .flatten()
        }

        /// The number of the newest call of the open run the block `fold` draws:
        /// Tab opens and folds that call's row while no member row is under the
        /// view.
        pub(crate) fn newest_member(&mut self, fold: u64) -> Option<u64> {
            self.with_open_run(fold, |_, group| group.newest_call())
                .flatten()
        }

        /// Answers what `ask` says of the open run the block `fold` draws, and
        /// nothing while the block is folded, holds no run, or the run has one
        /// call. The run of a block of the ring comes back with the fold state; a
        /// live run leaves the live area while it is asked.
        fn with_open_run<T>(
            &mut self,
            fold: u64,
            ask: impl FnOnce(&mut Self, &Group) -> T,
        ) -> Option<T> {
            if let Some(source) = self.screen.transcript().source(fold) {
                let frozen = source.downcast::<Frozen>().ok()?;
                let group = frozen.group()?;
                if group.calls < 2 || !self.block_open(fold, &frozen) {
                    return None;
                }
                return Some(ask(self, group));
            }
            let index = self.live.iter().position(|item| item.fold_id() == fold)?;
            let item = self.live.remove(index);
            let answer = match &item {
                Item::Group(group)
                    if group.calls > 1 && (group.going || self.folds.is_open(fold)) =>
                {
                    Some(ask(self, group))
                }
                _ => None,
            };
            self.live.insert(index, item);
            answer
        }

        /// The call whose row is `offset` rows into the open run `group`: the
        /// blank row above the block comes first, then the summary's padding row,
        /// its row and the padding row under it, and an open run draws no diff
        /// lines under its summary row.
        fn member_of(&mut self, group: &Group, offset: u16) -> Option<u64> {
            let row = group.row();
            let styles = self.entry_styles(&Block::Group(row));
            let mut rows = rows_for(0, u16::MAX, 0, self.transcript_width());
            rows.count = true;
            let mut null = Null;
            rows.blank = false;
            rows.air(&mut null);
            // A run that still goes and a run of one call draw no summary row.
            if !group.going && group.calls > 1 {
                // The rows of the summary card and the blank row under it.
                for _ in 0..=SUMMARY_ROWS {
                    let _ = rows.line(&mut null);
                }
                rows.blank = true;
            }
            row.members
                .member_at(&mut rows, &styles, &self.folds, true, offset)
        }

        /// Draws `entries` from row `y` down, the first `skip` rows left out and
        /// at most `limit` rows, and answers how many rows it drew.
        pub(crate) fn paint_blocks(
            &mut self,
            y: u16,
            skip: u16,
            limit: u16,
            entries: &[Entry<'_>],
            folds: &Folds,
        ) -> u16 {
            let width = self.transcript_width();
            let margin = self.margin();
            let full = self.text_width();
            let mut rows = super::rows_for(y, limit, skip, width);
            for entry in entries {
                let styles = self.entry_styles(&entry.block);
                let mut sink = GridSink {
                    screen: &mut self.screen,
                    width: full,
                    margin,
                };
                paint_entry(&mut rows, &mut sink, entry, folds, &styles);
            }
            rows.drawn
        }

        /// The rows `entries` take at the width of the screen, without drawing
        /// them, so a frame can lay the transcript out first.
        #[allow(dead_code, reason = "a frame lays the live rows out with `live_rows`")]
        pub(crate) fn blocks_rows(&mut self, entries: &[Entry<'_>], folds: &Folds) -> u16 {
            let width = self.transcript_width();
            let mut rows = super::rows_for(0, 0, 0, width);
            rows.count = true;
            for entry in entries {
                let styles = self.entry_styles(&entry.block);
                paint_entry(&mut rows, &mut super::Null, entry, folds, &styles);
            }
            rows.drawn
        }

        /// The style ids one block draws with, interned once before its rows are
        /// built: a cell of the block then writes an id and builds no style.
        pub(crate) fn entry_styles(&mut self, block: &Block<'_>) -> Styles {
            let state = match block {
                Block::Tool(tool) => tool.state,
                Block::Group(group) if group.failed > 0 => State::Failed,
                _ => State::Done,
            };
            let card = match block {
                Block::Tool(tool) => card(tool.name),
                _ => false,
            };
            let fill = match block {
                Block::Tool(tool) if card => Some(if tool.state == State::Failed {
                    Role::ErrFill
                } else {
                    Role::SurfaceFill
                }),
                Block::Steer { .. } | Block::NewLines { .. } => Some(Role::PanelFill),
                _ => None,
            };
            let paints = self.paints(fill, state);
            // A run's failed cards are tinted even though its summary row is not.
            let failed = match block {
                Block::Group(group) if group.failed > 0 => {
                    Some(self.paints(Some(Role::ErrFill), State::Failed))
                }
                _ => None,
            };
            let surface = matches!(block, Block::Group(_))
                .then(|| self.paints(Some(Role::SurfaceFill), State::Done));
            let edits = match block {
                Block::Tool(tool) => !tool.diff.is_empty(),
                Block::Group(group) => group.newest.is_some(),
                _ => false,
            };
            // A run's diffs draw in its cards, so their tints mix over the
            // surface.
            let under = if surface.is_some() {
                Some(Role::SurfaceFill)
            } else {
                fill
            };
            let diff = edits.then(|| self.diff_styles(under));
            Styles {
                paints,
                failed,
                surface,
                diff,
                card,
                wide: self.text_width() >= SIDE_BY_SIDE,
                italic: self.style(Role::Dim, Emphasis::DimItalic),
                bold: self.style(Role::Text, Emphasis::Bold),
            }
        }

        /// The styles a diff draws with over the tint of the block `fill`: a line
        /// of each kind, the gap fill of a half no line reaches, and the rule
        /// between the halves.
        fn diff_styles(&mut self, fill: Option<Role>) -> DiffStyles {
            DiffStyles {
                context: self.diff_line(fill, DiffKind::Context),
                added: self.diff_line(fill, DiffKind::Add),
                removed: self.diff_line(fill, DiffKind::Delete),
                gap: tint(&self.look.theme, fill, Role::Line, DIFF_GAP)
                    .map(|color| self.background(color)),
                rule: self.over(Role::Line, Emphasis::Plain, fill),
            }
        }

        /// The styles of one diff line: the tint of its kind, its number dim, its
        /// sign in the add or delete role, and its code in the roles of its
        /// tokens.
        fn diff_line(&mut self, fill: Option<Role>, kind: DiffKind) -> DiffLine {
            let (tint, sign) = match kind {
                DiffKind::Context => (None, Role::Text),
                DiffKind::Add => (
                    tint(&self.look.theme, fill, Role::Add, DIFF_TINT),
                    Role::Add,
                ),
                DiffKind::Delete => (
                    tint(&self.look.theme, fill, Role::Delete, DIFF_TINT),
                    Role::Delete,
                ),
            };
            DiffLine {
                tint: tint.map(|color| self.background(color)),
                number: self.over_tint(Role::Dim, tint, fill),
                sign: self.over_tint(sign, tint, fill),
                code: TOKENS.map(|token| self.over_tint(token_role(token), tint, fill)),
            }
        }

        /// The style id of `role` over `tint`, or over the tint of `fill` when the
        /// mode draws no tint of its own.
        fn over_tint(&mut self, role: Role, tint: Option<Color>, fill: Option<Role>) -> u32 {
            let Some(background) = tint else {
                return self.over(role, Emphasis::Plain, fill);
            };
            let mut style = self.look.theme.styled(role, Emphasis::Plain);
            style.bg = background;
            self.screen.intern(style)
        }

        /// The id of one color laid under the whole row, for a diff line's tint.
        fn background(&mut self, color: Color) -> u32 {
            self.screen.intern(Style {
                bg: color,
                ..Style::default()
            })
        }

        /// The styles of one tool or group block.
        fn paints(&mut self, fill: Option<Role>, state: State) -> Paints {
            let tint = match fill {
                Some(role) if self.has_fills() && self.look.theme.fill(role) != Color::Default => {
                    Some(self.style(role, Emphasis::Plain))
                }
                _ => None,
            };
            let glyph = match state {
                State::Running => self.over(Role::Warn, Emphasis::Plain, fill),
                State::Done => self.over(Role::Add, Emphasis::Plain, fill),
                State::Failed => self.over(Role::Error, Emphasis::Plain, fill),
            };
            let bar = match state {
                State::Running => self.over(Role::Warn, Emphasis::Plain, fill),
                State::Done => self.over(Role::Line, Emphasis::Plain, fill),
                State::Failed => self.over(Role::Error, Emphasis::Plain, fill),
            };
            Paints {
                fill: tint,
                glyph,
                bar,
                name: self.over(Role::Text, Emphasis::Bold, fill),
                text: self.over(Role::Text, Emphasis::Plain, fill),
                dim: self.over(Role::Dim, Emphasis::Plain, fill),
                add: self.over(Role::Add, Emphasis::Plain, fill),
                delete: self.over(Role::Delete, Emphasis::Plain, fill),
                accent: self.over(Role::Accent, Emphasis::Bold, fill),
                chip: self.chip(),
                error: self.over(Role::Error, Emphasis::Plain, fill),
            }
        }

        /// The style id of `role` under `emphasis`, drawn over the tint of
        /// `fill`: one interned id, so a cell over a tint is one array write and
        /// no color math.
        fn over(&mut self, role: Role, emphasis: Emphasis, fill: Option<Role>) -> u32 {
            let Some(fill) = fill else {
                return self.style(role, emphasis);
            };
            let background = self.look.theme.fill(fill);
            if background == Color::Default || background == self.look.theme.fill(role) {
                return self.style(role, emphasis);
            }
            let mut style = self.look.theme.styled(role, emphasis);
            style.bg = background;
            self.screen.intern(style)
        }

        /// The style of the ` error ` chip: the error color behind the text in
        /// rich mode, and on the text itself when the mode draws no fills, so
        /// the chip reads as an error either way.
        fn chip(&mut self) -> u32 {
            if !self.has_fills() {
                return self.style(Role::Error, Emphasis::Bold);
            }
            let style = Style {
                bg: self.look.theme.styled(Role::Error, Emphasis::Plain).fg,
                bold: true,
                ..Style::default()
            };
            self.screen.intern(style)
        }

        /// True when the theme draws fills at all: rich mode over a background
        /// the terminal answered. Terminal mode and `NO_COLOR` draw glyphs and
        /// weight only, so no block is tinted.
        fn has_fills(&self) -> bool {
            self.look.theme.fill(Role::OkFill) != Color::Default
        }
    }

    /// The style ids one block draws with, interned before its rows are built.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Styles {
        /// The ids a tool, a group, a steer and a new-lines row use.
        paints: Paints,
        /// The ids of a diff's lines, for a tool block that has one.
        diff: Option<DiffStyles>,
        /// True when the block draws as a card, its rows between a bar in column
        /// 0 and the fill of its state.
        card: bool,
        /// The ids a run's failed calls draw with, over the error fill, for the
        /// cards it keeps under its folded summary row.
        failed: Option<Paints>,
        /// The ids a run's summary row and its other cards draw with, over the
        /// surface fill: a run's own paints have no fill, since its plain rows
        /// have none.
        surface: Option<Paints>,
        /// True when the main area is wide enough for a diff's two halves.
        wide: bool,
        /// The dim italic of a thinking row.
        italic: u32,
        /// The bold body of a user turn.
        bold: u32,
    }

    /// The styles a diff draws with.
    #[derive(Clone, Copy, Debug)]
    struct DiffStyles {
        /// A line both sides keep.
        context: DiffLine,
        /// A line the edit added.
        added: DiffLine,
        /// A line the edit removed.
        removed: DiffLine,
        /// The fill of a half no line of that side reaches, when one is drawn.
        gap: Option<u32>,
        /// The rule between the two halves.
        rule: u32,
    }

    /// The styles of one diff line: its tint under the whole row, its number dim,
    /// its sign in the add or delete role, and its code in the roles of its
    /// tokens.
    #[derive(Clone, Copy, Debug)]
    struct DiffLine {
        /// The fill under the row, `None` in a mode that draws no fills.
        tint: Option<u32>,
        /// The line number.
        number: u32,
        /// The `+`, the `-`, or the two blanks of a context line.
        sign: u32,
        /// The code, one id per token in [`TOKENS`].
        code: [u32; TOKENS.len()],
    }

    impl DiffLine {
        /// The id of one token of the line's code.
        fn token(&self, token: Token) -> u32 {
            self.code[slot(token)]
        }
    }

    /// Every token a diff line's code may hold, in the order [`DiffLine`] keeps
    /// its style ids.
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

    /// Where `token`'s style id sits in [`DiffLine::code`].
    const fn slot(token: Token) -> usize {
        match token {
            Token::Text => 0,
            Token::Keyword => 1,
            Token::String => 2,
            Token::Comment => 3,
            Token::Number => 4,
            Token::Type => 5,
            Token::Function => 6,
            Token::Macro => 7,
            Token::Punct => 8,
            Token::Added => 9,
            Token::Removed => 10,
        }
    }

    /// The color of `role` mixed `percent` of the way into the block's `fill`:
    /// the tint of a diff line, and the fill of a half no line reaches.
    ///
    /// `None` where the mode draws no fill, and where the terminal keeps colors
    /// this build cannot mix: a palette entry rather than a direct color.
    fn tint(theme: &Theme, fill: Option<Role>, role: Role, percent: u8) -> Option<Color> {
        let (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) =
            (theme.fill(fill?), theme.styled(role, Emphasis::Plain).fg)
        else {
            return None;
        };
        Some(Rgb::new(r, g, b).mix(Rgb::new(tr, tg, tb), percent).color())
    }

    /// Where the rows of a block go: the grid of a frame, or the lines of a
    /// commit.
    pub(crate) trait Sink {
        /// Starts the run of a row, `y` in the grid.
        fn row(&mut self, y: u16);

        /// Lays `style` under the row from column 0 to the width.
        fn fill(&mut self, y: u16, style: u32);

        /// Lays `style` under the `width` columns of row `y` that start at `x`,
        /// under the text the row writes over them.
        fn fill_from(&mut self, y: u16, x: u16, width: u16, style: u32);

        /// Writes `text` at column `x` and answers the column after it.
        fn put(&mut self, y: u16, x: u16, text: &str, style: u32) -> u16;

        /// Writes `text` at column `x`, stopping at `limit`.
        fn put_until(&mut self, y: u16, x: u16, text: &str, style: u32, limit: u16) -> u16;

        /// Ends the block: no row is open any more.
        fn end(&mut self);

        /// The tail of a folded row's count: a row of the screen says the tab
        /// opens it, since a key still can.
        fn tail(&self) -> &'static str {
            MORE_HINT
        }
    }

    /// How one member of an open run hangs from the run's tree line.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Hang {
        /// The style of the line, which has no fill.
        rail: u32,
        /// The branch the member's first row of text starts with.
        edge: &'static str,
        /// Whether the member is the last of the run, whose branch ends the
        /// line.
        last: bool,
    }

    impl Hang {
        /// The tree line of one member, drawn in the bar of `styles`, which
        /// ends the run at that member when `last`.
        pub(crate) fn new(styles: &Styles, last: bool) -> Self {
            Hang {
                rail: styles.paints.bar,
                edge: if last { END } else { BRANCH },
                last,
            }
        }
    }

    /// What a row of an open run draws in the columns before its text.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Lead {
        /// The line goes on: `│` in column 0.
        Line,
        /// The line goes on until the member's first row of text, which starts
        /// with its branch.
        Branch,
        /// The last member has started: nothing.
        Blank,
    }

    /// The sink that draws the members of an open run: it shifts their columns
    /// in by [`INDENT`] and keeps the run's tree line in the columns before.
    pub(crate) struct Branch<'a> {
        /// The sink the rows go to.
        inner: &'a mut dyn Sink,
        /// The style of the tree line, which has no fill.
        rail: u32,
        /// The branch the member's first row of text starts with.
        edge: &'static str,
        /// Whether the member is the last of the run.
        last: bool,
        /// What the next row draws before its text.
        lead: Lead,
        /// The row whose lead is drawn already.
        drawn: Option<u16>,
        /// The open row whose lead waits for its text: the member's first row
        /// of text takes the branch, so the line goes in only once the row
        /// ends without it. A row of runs keeps the first text at a column.
        waiting: Option<u16>,
    }

    impl<'a> Branch<'a> {
        /// The sink that draws the rows of one member of an open run through
        /// `inner`.
        pub(crate) fn new(inner: &'a mut dyn Sink, hang: Hang) -> Self {
            Branch {
                inner,
                rail: hang.rail,
                edge: hang.edge,
                last: hang.last,
                lead: Lead::Branch,
                drawn: None,
                waiting: None,
            }
        }

        /// Draws the line of the open row when it ended with no text that
        /// takes the branch.
        pub(crate) fn settle(&mut self) {
            if let Some(y) = self.waiting.take() {
                self.line(y);
            }
        }

        /// The lead of a row that puts `text`: the branch on the member's first
        /// row of text, and the line on every other row.
        fn start(&mut self, y: u16, text: &str) {
            if self.lead == Lead::Branch {
                if text == BAR {
                    self.waiting = Some(y);
                    return;
                }
                self.waiting = None;
                self.inner.put(y, 0, self.edge, self.rail);
                self.lead = if self.last { Lead::Blank } else { Lead::Line };
                self.drawn = Some(y);
                return;
            }
            self.line(y);
        }

        /// The line in the columns before the text of row `y`, once per row.
        fn line(&mut self, y: u16) {
            if self.drawn == Some(y) || self.lead == Lead::Blank {
                return;
            }
            self.drawn = Some(y);
            self.inner.put(y, 0, RAIL, self.rail);
        }
    }

    impl Sink for Branch<'_> {
        fn row(&mut self, y: u16) {
            self.settle();
            self.inner.row(y);
            if self.lead == Lead::Branch {
                self.waiting = Some(y);
            } else {
                self.line(y);
            }
        }

        fn fill(&mut self, y: u16, style: u32) {
            self.inner.fill_from(y, INDENT, u16::MAX, style);
        }

        fn fill_from(&mut self, y: u16, x: u16, width: u16, style: u32) {
            self.inner
                .fill_from(y, x.saturating_add(INDENT), width, style);
        }

        fn put(&mut self, y: u16, x: u16, text: &str, style: u32) -> u16 {
            self.start(y, text);
            self.inner
                .put(y, x.saturating_add(INDENT), text, style)
                .saturating_sub(INDENT)
        }

        fn put_until(&mut self, y: u16, x: u16, text: &str, style: u32, limit: u16) -> u16 {
            self.start(y, text);
            self.inner
                .put_until(
                    y,
                    x.saturating_add(INDENT),
                    text,
                    style,
                    limit.saturating_add(INDENT),
                )
                .saturating_sub(INDENT)
        }

        fn end(&mut self) {
            self.settle();
            self.inner.end();
        }

        fn tail(&self) -> &'static str {
            self.inner.tail()
        }
    }

    /// The sink that draws a block into the grid of a frame.
    pub(crate) struct GridSink<'a> {
        /// The grid the rows go into.
        pub(crate) screen: &'a mut crate::screen::Screen,
        /// The columns of the main area, the margin included.
        pub(crate) width: u16,
        /// The blank columns a row keeps at each side of the main area.
        pub(crate) margin: u16,
    }

    impl GridSink<'_> {
        /// The columns the text of a row lays out in, the margin left out.
        fn inner(&self) -> u16 {
            self.width.saturating_sub(self.margin.saturating_mul(2))
        }

        /// The column of the screen the column `x` of a row sits at.
        fn column(&self, x: u16) -> u16 {
            self.margin.saturating_add(x)
        }
    }

    impl Sink for GridSink<'_> {
        fn row(&mut self, y: u16) {
            self.screen.clear_row(y);
        }

        fn fill(&mut self, y: u16, style: u32) {
            let width = self.width;
            let mut x = 0;
            while x < width {
                let next = self.screen.put_str_until(x, y, SPACES, style, width);
                if next <= x {
                    break;
                }
                x = next;
            }
        }

        fn fill_from(&mut self, y: u16, x: u16, width: u16, style: u32) {
            let limit = self.column(x.saturating_add(width)).min(self.width);
            let mut at = self.column(x);
            while at < limit {
                let next = self.screen.put_str_until(at, y, SPACES, style, limit);
                if next <= at {
                    break;
                }
                at = next;
            }
        }

        fn put(&mut self, y: u16, x: u16, text: &str, style: u32) -> u16 {
            let limit = self.column(self.inner());
            let next = self
                .screen
                .put_str_until(self.column(x), y, text, style, limit);
            next.saturating_sub(self.margin)
        }

        fn put_until(&mut self, y: u16, x: u16, text: &str, style: u32, limit: u16) -> u16 {
            let limit = self.column(limit.min(self.inner()));
            let next = self
                .screen
                .put_str_until(self.column(x), y, text, style, limit);
            next.saturating_sub(self.margin)
        }

        fn end(&mut self) {}
    }

    /// The sink that renders a block into the lines of a commit: the text of
    /// every row goes into one buffer, so a line is a range of it, and the gaps
    /// between the runs of a row are the spaces the row draws.
    #[derive(Debug, Default)]
    pub(crate) struct LineSink {
        /// The text of every run, in the order the lines take it.
        text: String,
        /// One run of a line: where its text starts, how long it is, its style.
        runs: Vec<(u32, u32, u32)>,
        /// The range of `runs` each line takes.
        lines: Vec<Range<usize>>,
        /// The runs of the row being built: the column, where its text starts in
        /// `text`, how long it is, and its style.
        row: Vec<(u16, u32, u32, u32)>,
        /// The style under the row, when it has one.
        fill: Option<u32>,
        /// The fills a block laid under parts of the row being built: the column,
        /// the columns it covers, and its style.
        fills: Vec<(u16, u16, u32)>,
        /// The columns of the main area, the margin included.
        width: u16,
        /// The blank columns a row keeps at each side of the main area.
        margin: u16,
        /// Whether a row is open.
        started: bool,
        /// Whether a folded row says a tab opens it: false for the lines of
        /// inline mode, which are printed once and never change.
        hint: bool,
    }

    impl LineSink {
        /// Empties the sink for a commit of `width` columns with `margin` blank
        /// columns at each side, keeping the buffers.
        pub(crate) fn clear(&mut self, width: u16, margin: u16, hint: bool) {
            self.text.clear();
            self.runs.clear();
            self.lines.clear();
            self.row.clear();
            self.fills.clear();
            self.fill = None;
            self.width = width;
            self.margin = margin;
            self.started = false;
            self.hint = hint;
        }

        /// The columns the text of a row lays out in, the margin left out.
        fn inner(&self) -> u16 {
            self.width.saturating_sub(self.margin.saturating_mul(2))
        }

        /// The column of the main area the column `x` of a row sits at.
        fn column(&self, x: u16) -> u16 {
            self.margin.saturating_add(x)
        }

        /// How many lines the sink holds so far.
        pub(crate) fn line_count(&self) -> usize {
            self.lines.len()
        }

        /// The runs of every line, and the range of them that makes each line.
        pub(crate) fn runs(&self) -> Runs<'_> {
            Runs {
                runs: self
                    .runs
                    .iter()
                    .map(|&(start, len, style)| Run {
                        text: slice(&self.text, start, len),
                        style,
                    })
                    .collect(),
                spans: self.lines.clone(),
            }
        }

        /// Opens a run of `text`, which a caller that pushes a whole line itself
        /// closes with [`LineSink::line_from`].
        pub(crate) fn run(&mut self, text: &str, style: u32) {
            let start = self.text.len() as u32;
            self.text.push_str(text);
            self.runs.push((start, text.len() as u32, style));
        }

        /// The end of the runs of a whole line that started at `start`.
        pub(crate) fn line_from(&mut self, start: usize) {
            self.lines.push(start..self.runs.len());
        }

        /// The number of runs pushed so far, for a line that starts here.
        pub(crate) fn at(&mut self) -> usize {
            self.flush();
            self.runs.len()
        }

        /// One line of `text` in `style`, the way a plain line of the transcript
        /// above the live area prints. A line wider than the row is cut, so it
        /// never reaches the margin at the right.
        pub(crate) fn text_line(&mut self, text: &str, style: u32) {
            let start = self.at();
            self.margin_start();
            self.run(cut_to(text, self.inner()), style);
            self.line_from(start);
        }

        /// The columns of the blank margin at the start of every line.
        pub(crate) fn margin(&self) -> u16 {
            self.margin
        }

        /// The blank margin at the start of a line a caller pushes whole.
        pub(crate) fn margin_start(&mut self) {
            if self.margin > 0 {
                self.gap(self.margin, 0);
            }
        }

        /// Lays the fills of the row under the columns `from` to `to`: the fill a
        /// block laid under a part of the row, else the row's own.
        fn under(&mut self, from: usize, to: usize, fill: Option<u32>) {
            let mut at = from;
            while at < to {
                let (end, style) = self.fill_at(at, fill);
                let end = end.clamp(at + 1, to);
                self.gap((end - at) as u16, style);
                at = end;
            }
        }

        /// The fill under column `at` and the column it reaches to.
        fn fill_at(&self, at: usize, fill: Option<u32>) -> (usize, u32) {
            for &(x, width, style) in &self.fills {
                let start = usize::from(x);
                if at >= start && at < start + usize::from(width) {
                    return (start + usize::from(width), style);
                }
            }
            (usize::from(self.width), fill.unwrap_or(0))
        }

        /// The widest a filled gap may be: the run of spaces one row writes.
        fn gap(&mut self, columns: u16, style: u32) {
            let mut left = usize::from(columns);
            while left > 0 {
                let take = left.min(SPACES.len());
                let start = self.text.len() as u32;
                self.text.push_str(&SPACES[..take]);
                self.runs.push((start, take as u32, style));
                left -= take;
            }
        }

        /// The bytes of one row's run that the line takes: the part after the
        /// columns the line already used, and the part that fits in `width`.
        fn clip(
            &self,
            at: u32,
            len: u32,
            column: usize,
            used: usize,
            width: usize,
        ) -> (usize, usize) {
            let text = slice(&self.text, at, len);
            let mut cut = 0;
            if column < used {
                cut = columns_in(text, used - column);
            }
            if column + cut > width {
                cut = cut.min(width.saturating_sub(column));
            }
            let room = width.saturating_sub(column.max(used)) as u16;
            (cut, cut + cut_to(&text[cut..], room).len())
        }

        /// Closes the open row into a line of runs: the row's text in column
        /// order, the gaps between it the fill's spaces, and no overlap.
        fn flush(&mut self) {
            if !self.started {
                return;
            }
            self.started = false;
            let width = usize::from(self.width);
            let fill = self.fill;
            let start = self.runs.len();
            let mut used = 0usize;
            let mut row = std::mem::take(&mut self.row);
            while !row.is_empty() {
                let at = row
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, item)| item.0)
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                let (column, offset, len, style) = row.swap_remove(at);
                let column = usize::from(column).min(width);
                let (cut, end) = self.clip(offset, len, column, used, width);
                if end > cut {
                    if column > used {
                        self.under(used, column, fill);
                    }
                    self.runs
                        .push((offset + cut as u32, (end - cut) as u32, style));
                    used =
                        column.max(used) + column_count(&slice(&self.text, offset, len)[cut..end]);
                }
                if used >= width {
                    break;
                }
            }
            // A fill that starts at a column, a card hung from a run's tree
            // line, reaches the row's end as a whole-row fill does.
            if used < width && (fill.is_some() || !self.fills.is_empty()) {
                self.under(used, width, fill);
            }
            row.clear();
            self.row = row;
            self.fills.clear();
            self.lines.push(start..self.runs.len());
            self.fill = None;
        }
    }

    impl Sink for LineSink {
        fn row(&mut self, _y: u16) {
            self.flush();
            self.started = true;
        }

        fn fill(&mut self, _y: u16, style: u32) {
            self.fill = Some(style);
        }

        fn fill_from(&mut self, _y: u16, x: u16, width: u16, style: u32) {
            // The runs of a row keep their columns with the margin, so its
            // fills do too.
            self.fills.push((self.column(x), width, style));
        }

        fn put(&mut self, _y: u16, x: u16, text: &str, style: u32) -> u16 {
            let room = self.inner().saturating_sub(x);
            let text = cut_to(text, room);
            let start = self.text.len() as u32;
            self.text.push_str(text);
            self.row
                .push((self.column(x), start, text.len() as u32, style));
            x + columns(text)
        }

        fn put_until(&mut self, _y: u16, x: u16, text: &str, style: u32, limit: u16) -> u16 {
            let room = limit.min(self.inner()).saturating_sub(x);
            let text = cut_to(text, room);
            let start = self.text.len() as u32;
            self.text.push_str(text);
            self.row
                .push((self.column(x), start, text.len() as u32, style));
            x + columns(text)
        }

        fn end(&mut self) {
            self.flush();
        }

        fn tail(&self) -> &'static str {
            if self.hint { MORE_HINT } else { MORE }
        }
    }

    /// The runs of a commit and the range of them that makes each line.
    pub(crate) struct Runs<'a> {
        /// The runs of every line.
        pub(crate) runs: Vec<Run<'a>>,
        /// The range of `runs` each line takes.
        pub(crate) spans: Vec<Range<usize>>,
    }

    /// The text of one run of a [`LineSink`].
    fn slice(text: &str, start: u32, len: u32) -> &str {
        let start = start as usize;
        let end = (start + len as usize).min(text.len());
        text.get(start..end).unwrap_or_default()
    }

    /// The columns `text` takes.
    fn column_count(text: &str) -> usize {
        text.chars().map(cells).sum()
    }

    /// The bytes of `text` that cover `columns` columns, at least one char when
    /// it has any.
    fn columns_in(text: &str, columns: usize) -> usize {
        let mut used = 0;
        for (at, c) in text.char_indices() {
            if used >= columns {
                return at;
            }
            used += cells(c);
        }
        text.len()
    }

    /// The first bytes of `text` that fit in `columns` columns.
    fn cut_to(text: &str, columns: u16) -> &str {
        let columns = usize::from(columns);
        let mut used = 0;
        for (at, c) in text.char_indices() {
            let width = cells(c);
            if used + width > columns {
                return &text[..at];
            }
            used += width;
        }
        text
    }

    /// Draws one block's rows.
    ///
    /// A run of tool rows keeps one blank row above it and one below, the air
    /// that sets it apart from the text around it, and takes no second one from a
    /// neighbour that drew one already.
    pub(crate) fn paint_entry(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        entry: &Entry<'_>,
        folds: &Folds,
        styles: &Styles,
    ) {
        let open = folds.is_open(entry.id);
        let tool = matches!(entry.block, Block::Tool(_) | Block::Group(_));
        if tool {
            rows.air(sink);
        }
        match entry.block {
            Block::User { text } => paint_user(rows, sink, text, styles),
            Block::Thinking { seconds, text } => {
                paint_thinking(rows, sink, seconds, text, open, styles);
            }
            Block::Tool(tool) => {
                let shown = folds.diff_shown(entry.id, tool.diff.len(), open);
                paint_tool(rows, sink, &tool, open, shown, styles);
            }
            Block::Group(group) => {
                let shown = group
                    .newest
                    .map_or(0, |edit| folds.diff_shown(edit.fold, edit.diff.len(), open));
                paint_group(rows, sink, &group, folds, open, shown, styles);
            }
            Block::Rule(usage) => paint_rule(rows, sink, usage, styles),
            Block::Compacted(row) => paint_compacted(rows, sink, &row, open, styles),
            Block::Steer { text, queued } => paint_steer(rows, sink, text, queued, styles),
            Block::NewLines { count } => paint_new_lines(rows, sink, count, styles),
            Block::Picture {
                cells,
                placeholder,
                columns,
                height,
                chip,
            } => paint_picture(
                rows,
                sink,
                cells,
                placeholder,
                columns,
                height,
                chip,
                styles,
            ),
        }
        if tool {
            rows.air(sink);
        }
    }

    /// A user turn between blank rows: `›` in the accent and the message bold,
    /// wrapping under a two-column indent. No fill, so the terminal's own
    /// background shows through.
    fn paint_user(rows: &mut Rows, sink: &mut dyn Sink, text: &str, styles: &Styles) {
        rows.air(sink);
        let width = rows.width.saturating_sub(2);
        let mut first = true;
        for piece in Wrap::new(text, width) {
            if let Some(y) = rows.line(sink) {
                if first {
                    sink.put(y, 0, "›", styles.paints.accent);
                }
                sink.put(y, 2, piece, styles.bold);
            }
            first = false;
        }
        rows.air(sink);
    }

    /// One picture: a row of `▀` cells for every two rows of pixels, each in a
    /// style that puts the top pixel in its foreground and the low one in its
    /// background, then the dim chip that names the image.
    ///
    /// A picture with a `placeholder` style draws its rows as the placeholder
    /// cells that name the image the terminal already holds instead: one style
    /// for every cell, so a scroll of the ring moves them as it moves text.
    ///
    /// The rows are finished when they are pushed, so the block has no fold and
    /// no live state, and every cell's style was interned before its row was
    /// built. Neighbouring cells of one style share one run, so a gradient of
    /// few colors costs few runs.
    #[expect(clippy::too_many_arguments, reason = "one block draws one picture")]
    fn paint_picture(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        cells: &[u32],
        placeholder: Option<u32>,
        columns: u16,
        height: u16,
        chip: &str,
        styles: &Styles,
    ) {
        let across = usize::from(columns.min(rows.width.saturating_sub(2)));
        let stride = usize::from(columns);
        let mut run = String::new();
        for row in 0..usize::from(height) {
            let Some(y) = rows.line(sink) else {
                continue;
            };
            if let Some(style) = placeholder {
                run.clear();
                let row = u16::try_from(row).unwrap_or(u16::MAX);
                let across = u16::try_from(across).unwrap_or(u16::MAX);
                picture_native::placeholder_row(&mut run, row, across);
                sink.put(y, super::PICTURE_LEFT, &run, style);
                continue;
            }
            let start = row.saturating_mul(stride);
            let (mut column, mut x) = (0, super::PICTURE_LEFT);
            while column < across {
                let Some(style) = cells.get(start.saturating_add(column)).copied() else {
                    break;
                };
                run.clear();
                while column < across {
                    let Some(next) = cells.get(start.saturating_add(column)).copied() else {
                        break;
                    };
                    if next != style {
                        break;
                    }
                    run.push('▀');
                    column += 1;
                }
                x = sink.put(y, x, &run, style);
            }
        }
        if let Some(y) = rows.line(sink) {
            sink.put(y, 2, chip, styles.paints.dim);
        }
    }

    /// A thinking block between blank rows: one dim italic row with the time and
    /// the fold marker, and the newest line of the reasoning under it while it is
    /// open.
    ///
    /// A thought under a second reads `thought` with no time. One that left no
    /// text either says nothing, so it draws no row and no blank rows either.
    pub(super) fn paint_thinking(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        seconds: u32,
        text: &str,
        open: bool,
        styles: &Styles,
    ) {
        if seconds == 0 && text.is_empty() {
            return;
        }
        let dim = styles.italic;
        rows.air(sink);
        let mut buf = [0u8; DIGITS];
        if let Some(y) = rows.line(sink) {
            let mut x = sink.put(y, 0, "thought", dim);
            if seconds == 0 {
                x = sink.put(y, x, "  ", dim);
            } else {
                let secs = digits(&mut buf, u64::from(seconds));
                x = sink.put(y, x, " for ", dim);
                x = sink.put(y, x, secs, dim);
                x = sink.put(y, x, "s  ", dim);
            }
            sink.put(y, x, if open { "▾" } else { "▸" }, dim);
        }
        if open {
            for piece in Wrap::new(text, rows.width) {
                if let Some(y) = rows.line(sink) {
                    sink.put(y, 0, piece, dim);
                }
            }
        }
        rows.air(sink);
    }

    /// A compaction of the history: one dim row with the tokens it took off, the
    /// messages its checkpoint stands in for, and the mark of its fold, and
    /// under it the checkpoint text while the row is open.
    fn paint_compacted(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        row: &CompactedRow<'_>,
        open: bool,
        styles: &Styles,
    ) {
        let dim = styles.paints.dim;
        rows.air(sink);
        let mut buf = [0u8; DIGITS];
        if let Some(y) = rows.line(sink) {
            let mut x = sink.put(y, 0, "compacted", dim);
            if let Some((before, after)) = row.tokens {
                x = sink.put(y, x, " · ", dim);
                let text = tokens(&mut buf, before);
                x = sink.put(y, x, text, dim);
                x = sink.put(y, x, " → ", dim);
                let text = tokens(&mut buf, after);
                x = sink.put(y, x, text, dim);
                x = sink.put(y, x, " tokens", dim);
            }
            x = sink.put(y, x, " · ", dim);
            let count = digits(&mut buf, u64::from(row.messages));
            x = sink.put(y, x, count, dim);
            x = sink.put(y, x, " messages  ", dim);
            sink.put(y, x, if open { "▾" } else { "▸" }, dim);
        }
        if open && let Some(text) = row.text {
            for piece in Wrap::new(text, rows.width) {
                if let Some(y) = rows.line(sink) {
                    sink.put(y, 0, piece, styles.paints.text);
                }
            }
        }
        rows.air(sink);
    }

    /// A tool call: a card for the tools [`card`] names, and a plain row for
    /// every other tool.
    ///
    /// A card draws its title and under it the body of what the call did: its
    /// diff, the error it failed with, or the tail of the output it printed. A
    /// plain row draws its row, the diff under it that `shown` holds, and the
    /// whole of its error or the tail of its output while it is open.
    pub(super) fn paint_tool(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        tool: &ToolRow<'_>,
        open: bool,
        shown: usize,
        styles: &Styles,
    ) {
        if card(tool.name) {
            paint_card(rows, sink, tool, open, shown, styles);
            return;
        }
        paint_plain_row(rows, sink, tool, open, styles);
        paint_diff(rows, sink, styles, shown, tool.diff);
        if open {
            let shown = if let Some(error) = tool.error {
                paint_error(rows, sink, error, styles);
                !error.is_empty()
            } else if tool.diff.is_empty()
                && let Some(output) = tool.output
            {
                paint_output(rows, sink, output, tool.earlier, styles);
                !output.is_empty()
            } else {
                false
            };
            // The next member of an open run would sit on the last line.
            if shown {
                rows.air(sink);
            }
        }
    }

    /// A card of a call: a blank row with no fill, a padding row, its title with
    /// the result at the right edge, a padding row under the title, its body, and
    /// a last padding row, every row of the card on the fill of the call's state
    /// with its bar in column 0. The padding row under the title draws only when
    /// the body draws a line.
    ///
    /// The body is the diff of an edit or of a write that has one, the last
    /// [`BODY_LINES`] lines of the error a call failed with, or the last
    /// [`BODY_LINES`] lines of the output a `bash`, `codemode` or `write` call
    /// printed: the whole of either and no
    /// count row while the card is open. A card whose body shows no line draws
    /// its title between two padding rows.
    ///
    /// A card keeps one blank row between itself and the row before it and the
    /// row after it, the air that sets it apart from the rest of the run.
    fn paint_card(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        tool: &ToolRow<'_>,
        open: bool,
        shown: usize,
        styles: &Styles,
    ) {
        let body = has_body(tool, shown);
        rows.air(sink);
        pad(rows, sink, styles);
        paint_title(rows, sink, tool, open, styles);
        if body {
            pad(rows, sink, styles);
        }
        if tool.state == State::Failed {
            if let Some(error) = tool.error
                && !error.is_empty()
            {
                paint_body(rows, sink, error, 0, open, styles);
            }
        } else if tool.name == "edit" || !tool.diff.is_empty() {
            paint_diff(rows, sink, styles, shown, tool.diff);
        } else if let Some(output) = tool.output
            && !output.is_empty()
        {
            paint_body(rows, sink, output, tool.earlier, open, styles);
        }
        pad(rows, sink, styles);
        rows.air(sink);
    }

    /// Whether a card of `tool` draws a line of body under its title: the lines
    /// of its diff or of a write's, the error it failed with, or the tail of what
    /// it printed.
    fn has_body(tool: &ToolRow<'_>, shown: usize) -> bool {
        if tool.state == State::Failed {
            return tool.error.is_some_and(|error| !error.is_empty());
        }
        if tool.name == "edit" || !tool.diff.is_empty() {
            return shown > 0 && !tool.diff.is_empty();
        }
        tool.output.is_some_and(|output| !output.is_empty())
    }

    /// The title row of a card: the dim mark of the tool and its target at
    /// column 2, behind the cross of a call that failed, and the result pushed
    /// to the right edge of the main area.
    fn paint_title(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        tool: &ToolRow<'_>,
        open: bool,
        styles: &Styles,
    ) {
        let paints = &styles.paints;
        let marker = if open { "▾" } else { "▸" };
        let right = tool
            .right
            .iter()
            .fold(2 + columns(marker), |sum, span| sum + columns(span.text));
        let Some(y) = card_row(rows, sink, styles) else {
            return;
        };
        let width = rows.width;
        let limit = width.saturating_sub(right).saturating_sub(1);
        let mut x = 2;
        if tool.state == State::Failed {
            x = sink.put_until(y, x, "✗ ", paints.glyph, limit);
        }
        let x = sink.put_until(y, x, mark(tool.name), paints.dim, limit);
        sink.put_until(y, x, tool.target, paints.text, limit);
        let mut x = width.saturating_sub(right);
        for span in tool.right {
            let style = match span.role {
                SpanRole::Add => paints.add,
                SpanRole::Delete => paints.delete,
                SpanRole::Dim => paints.dim,
                SpanRole::Text => paints.text,
            };
            x = sink.put_until(y, x, span.text, style, width);
        }
        sink.put_until(y, x.saturating_add(2), marker, paints.dim, width);
    }

    /// The body of a card: one row per line of the tail of its output, at column
    /// 2 and cut to the width, and under them the count of the lines the card
    /// leaves out, while it is folded. An open card draws the whole tail and no
    /// count.
    fn paint_body(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        output: &str,
        earlier: u64,
        open: bool,
        styles: &Styles,
    ) {
        let paints = &styles.paints;
        let start = if open {
            0
        } else {
            tail_start(output, BODY_LINES)
        };
        for line in output[start..].lines() {
            let Some(y) = card_row(rows, sink, styles) else {
                continue;
            };
            sink.put_until(y, 2, line, paints.text, rows.width);
        }
        if open {
            return;
        }
        let cut = earlier.saturating_add(output[..start].lines().count() as u64);
        if cut == 0 {
            return;
        }
        let Some(y) = card_row(rows, sink, styles) else {
            return;
        };
        let mut buf = [0u8; DIGITS];
        let count = digits(&mut buf, cut);
        let x = sink.put_until(y, 2, count, paints.dim, rows.width);
        let tail = sink.tail();
        sink.put_until(y, x, tail, paints.dim, rows.width);
    }

    /// One row of a card: the surface fill across the main area and the bar of
    /// the call's state in column 0. Answers the row it draws at.
    fn card_row(rows: &mut Rows, sink: &mut dyn Sink, styles: &Styles) -> Option<u16> {
        card_edge(rows, sink, styles, BAR)
    }

    /// One row of a card that starts with `edge` in column 0: the bar of a row,
    /// and the line an open run hangs its members from. Answers the row it draws
    /// at.
    fn card_edge(rows: &mut Rows, sink: &mut dyn Sink, styles: &Styles, edge: &str) -> Option<u16> {
        let paints = &styles.paints;
        let y = rows.line(sink)?;
        if let Some(fill) = paints.fill {
            sink.fill(y, fill);
        }
        sink.put_until(y, 0, edge, paints.bar, rows.width);
        Some(y)
    }

    /// A padding row of a card: the surface and the bar alone.
    fn pad(rows: &mut Rows, sink: &mut dyn Sink, styles: &Styles) {
        let _ = card_row(rows, sink, styles);
    }

    /// The blank row between an open run's summary card and its first member,
    /// with no fill and the tree line in column 0.
    fn rail_air(rows: &mut Rows, sink: &mut dyn Sink, rail: u32) {
        if let Some(y) = rows.line(sink) {
            sink.put_until(y, 0, RAIL, rail, rows.width);
        }
        rows.blank = true;
    }

    /// A padding row of a card that starts with `edge` in column 0.
    fn pad_edge(rows: &mut Rows, sink: &mut dyn Sink, styles: &Styles, edge: &str) {
        let _ = card_edge(rows, sink, styles, edge);
    }

    /// The tail of the output a call printed, under its row while the call is
    /// open: the count of the lines it leaves out over its last [`OUTPUT_ROWS`]
    /// lines, each cut to the width at the call row's indent and dim over the
    /// block's tint. A short output draws whole and has no count row.
    fn paint_output(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        output: &str,
        earlier: u64,
        styles: &Styles,
    ) {
        let paints = &styles.paints;
        let total = output.lines().count();
        let shown = total.min(OUTPUT_ROWS);
        let cut = earlier.saturating_add((total - shown) as u64);
        if cut > 0
            && let Some(y) = rows.line(sink)
        {
            if let Some(fill) = paints.fill {
                sink.fill(y, fill);
            }
            let mut buf = [0u8; DIGITS];
            let x = sink.put_until(y, 2, digits(&mut buf, cut), paints.dim, rows.width);
            sink.put_until(y, x, " earlier lines", paints.dim, rows.width);
        }
        for line in output.lines().skip(total - shown) {
            let Some(y) = rows.line(sink) else {
                continue;
            };
            if let Some(fill) = paints.fill {
                sink.fill(y, fill);
            }
            sink.put_until(y, 2, line, paints.dim, rows.width);
        }
    }

    /// The error a failed call ended with, under its row while the call is open:
    /// the message over the block's tint, one row per piece it wraps into, under
    /// the call row's two-column indent.
    fn paint_error(rows: &mut Rows, sink: &mut dyn Sink, error: &str, styles: &Styles) {
        let style = styles.paints.error;
        let width = rows.width.saturating_sub(2);
        for line in error.lines() {
            for piece in Wrap::new(line, width) {
                let Some(y) = rows.line(sink) else {
                    continue;
                };
                if let Some(fill) = styles.paints.fill {
                    sink.fill(y, fill);
                }
                sink.put_until(y, 2, piece, style, rows.width);
            }
        }
    }

    /// True for the tools that draw as a card: `bash`, `codemode`, `write` and
    /// `edit`. Every other tool, a read, a search and an MCP call, draws as a
    /// plain row.
    pub(crate) fn card(name: &str) -> bool {
        matches!(name, "bash" | "codemode" | "write" | "edit")
    }

    /// The styles a call of a run draws with: a card over the error fill when
    /// the call failed and over the surface otherwise, and a plain row in the
    /// run's own paints.
    pub(crate) fn member_styles(styles: &Styles, name: &str, state: State) -> Styles {
        if !card(name) {
            return *styles;
        }
        let paints = if state == State::Failed {
            styles.failed
        } else {
            styles.surface
        };
        match paints {
            Some(paints) => Styles {
                paints,
                card: true,
                ..*styles
            },
            None => *styles,
        }
    }

    /// The styles of a run's summary row: a one-row card over the error fill
    /// when a call of the run failed, and over the surface otherwise.
    fn summary_styles(styles: &Styles, failed: bool) -> Styles {
        let paints = if failed {
            styles.failed
        } else {
            styles.surface
        };
        Styles {
            paints: paints.unwrap_or(styles.paints),
            card: true,
            ..*styles
        }
    }

    /// The dim mark a card's title starts with: the shell's `$`, the pen of a
    /// write, the arrow of an edit and the diamond of a script.
    fn mark(name: &str) -> &'static str {
        match name {
            "write" => "\u{2190} Write ",
            "edit" => "\u{2190} Edit ",
            "codemode" => "\u{25c6} Codemode ",
            _ => "$ ",
        }
    }

    /// The byte the last `keep` lines of `text` start at, and 0 while it has
    /// fewer: the newlines are found from the end, so no line is counted twice.
    fn tail_start(text: &str, keep: usize) -> usize {
        let body = text.strip_suffix('\n').unwrap_or(text);
        let mut at = body.len();
        for _ in 0..keep {
            match body[..at].rfind('\n') {
                Some(found) => at = found,
                None => return 0,
            }
        }
        at + 1
    }

    /// The icon a plain row starts with: the arrow of a read, the star of a
    /// search, and the diamond of any other tool.
    fn icon(name: &str) -> &'static str {
        match name {
            "read" => "→",
            "grep" | "find" => "✱",
            _ => "◇",
        }
    }

    /// A plain row: the tool's icon and its name upper-cased and dim, its target
    /// in the text role, and its result pushed to the right edge, over no tint.
    fn paint_plain_row(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        tool: &ToolRow<'_>,
        open: bool,
        styles: &Styles,
    ) {
        let paints = &styles.paints;
        let marker = if open { "▾" } else { "▸" };
        let right = tool
            .right
            .iter()
            .fold(2 + columns(marker), |sum, span| sum + columns(span.text));
        let Some(y) = rows.line(sink) else {
            return;
        };
        let width = rows.width;
        let limit = width.saturating_sub(right).saturating_sub(1);
        let mut buf = [0u8; CASE];
        let (first, rest) = capital(&mut buf, tool.name);
        let quoted = matches!(tool.name, "grep" | "find");
        let quote = if quoted { "\"" } else { "" };
        let mut x = sink.put_until(y, 0, icon(tool.name), paints.dim, limit);
        x = sink.put_until(y, x, " ", paints.dim, limit);
        x = sink.put_until(y, x, first, paints.dim, limit);
        x = sink.put_until(y, x, rest, paints.dim, limit);
        x = sink.put_until(y, x, " ", paints.text, limit);
        x = sink.put_until(y, x, quote, paints.text, limit);
        x = sink.put_until(y, x, tool.target, paints.text, limit);
        sink.put_until(y, x, quote, paints.text, limit);
        let result = if tool.state == State::Failed {
            paints.error
        } else {
            paints.dim
        };
        let mut x = width.saturating_sub(right);
        for span in tool.right {
            x = sink.put_until(y, x, span.text, result, width);
        }
        sink.put_until(y, x.saturating_add(2), marker, paints.dim, width);
    }

    /// The bar of a card in column 0 of row `y`.
    ///
    /// A diff line of a card is put after the line's tint, so the tint does not
    /// paint over the bar.
    fn card_bar(sink: &mut dyn Sink, y: u16, width: u16, styles: &Styles) {
        if styles.card {
            sink.put_until(y, 0, BAR, styles.paints.bar, width);
        }
    }

    /// The diff under an edit: each line's code in the roles of its tokens over
    /// the tint of its line, its number dim and its sign in the add or delete
    /// role.
    ///
    /// A wide main area draws the old lines at the left and the new lines at the
    /// right, a context line on the same row of both and a line without a
    /// partner on the gap fill of the other half. A block shows the first few
    /// lines of the diff and says how many the tab opens, or none of them while a
    /// newer edit has one.
    fn paint_diff(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        styles: &Styles,
        shown: usize,
        diff: &[OwnedDiff],
    ) {
        let Some(diff_styles) = &styles.diff else {
            return;
        };
        if shown == 0 {
            return;
        }
        let taken = diff.iter().take(shown).map(OwnedDiff::line);
        if styles.wide {
            paint_halves(rows, sink, styles, diff_styles, taken);
        } else {
            for line in taken {
                let Some(y) = rows.line(sink) else {
                    continue;
                };
                if let Some(fill) = styles.paints.fill {
                    sink.fill(y, fill);
                }
                paint_line(
                    sink,
                    y,
                    0,
                    rows.width,
                    &line,
                    line_styles(diff_styles, line.kind),
                );
                card_bar(sink, y, rows.width, styles);
            }
        }
        let more = diff.len().saturating_sub(shown);
        if more == 0 {
            return;
        }
        let Some(y) = rows.line(sink) else {
            return;
        };
        let paints = &styles.paints;
        if let Some(fill) = paints.fill {
            sink.fill(y, fill);
        }
        let mut buf = [0u8; DIGITS];
        let count = digits(&mut buf, more as u64);
        // A card keeps its count beside the bar, at the left of its body; a row
        // under a call keeps it under the sign of its lines.
        let x = sink.put(y, if styles.card { 2 } else { SIGN }, count, paints.dim);
        let tail = sink.tail();
        sink.put_until(y, x, tail, paints.dim, rows.width);
        card_bar(sink, y, rows.width, styles);
    }

    /// The styles of the diff lines of one kind.
    fn line_styles(lines: &DiffStyles, kind: DiffKind) -> &DiffLine {
        match kind {
            DiffKind::Context => &lines.context,
            DiffKind::Add => &lines.added,
            DiffKind::Delete => &lines.removed,
        }
    }

    /// The two halves of a wide diff: the old lines at the left, the new lines at
    /// the right, and the rule between them.
    ///
    /// A line of one side is paired with the line of the other side that sits at
    /// the same place, so a context line lands on both halves of one row, and a
    /// side that runs out leaves the gap fill.
    fn paint_halves<'a, I>(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        styles: &Styles,
        lines: &DiffStyles,
        diff: I,
    ) where
        I: Iterator<Item = Diff<'a>> + Clone,
    {
        let width = rows.width;
        let half = width.saturating_sub(3) / 2;
        let rule = half.saturating_add(1);
        let right = rule.saturating_add(2);
        let right_width = width.saturating_sub(right);
        let mut old = diff.clone().filter(|line| line.kind != DiffKind::Add);
        let mut new = diff.filter(|line| line.kind != DiffKind::Delete);
        loop {
            let pair = (old.next(), new.next());
            if pair.0.is_none() && pair.1.is_none() {
                break;
            }
            let Some(y) = rows.line(sink) else {
                continue;
            };
            if let Some(fill) = styles.paints.fill {
                sink.fill(y, fill);
            }
            for (line, x, columns) in [(pair.0, 0, half), (pair.1, right, right_width)] {
                match line {
                    Some(line) => {
                        paint_line(sink, y, x, columns, &line, line_styles(lines, line.kind))
                    }
                    None => {
                        if let Some(gap) = lines.gap {
                            sink.fill_from(y, x, columns, gap);
                        }
                    }
                }
            }
            sink.put_until(y, rule, "\u{2502}", lines.rule, width);
            card_bar(sink, y, width, styles);
        }
    }

    /// One diff line in the columns `x` to `x + width`: the tint of its kind
    /// under them, its number dim, its sign, and its code in its token roles.
    fn paint_line(
        sink: &mut dyn Sink,
        y: u16,
        x: u16,
        width: u16,
        diff: &Diff<'_>,
        line: &DiffLine,
    ) {
        let limit = x.saturating_add(width);
        if let Some(tint) = line.tint {
            sink.fill_from(y, x, width, tint);
        }
        let mut buf = [0u8; DIGITS];
        let number = right_in(&mut buf, u64::from(diff.number), 4);
        sink.put_until(y, x.saturating_add(3), number, line.number, limit);
        let sign = match diff.kind {
            DiffKind::Add => "+ ",
            DiffKind::Delete => "- ",
            DiffKind::Context => "  ",
        };
        let mut at = sink.put_until(y, x.saturating_add(SIGN), sign, line.sign, limit);
        if diff.tokens.is_empty() {
            sink.put_until(y, at, diff.text, line.token(Token::Text), limit);
            return;
        }
        for span in diff.tokens {
            let Some(text) = diff.text.get(span.start as usize..span.end as usize) else {
                continue;
            };
            at = sink.put_until(y, at, text, line.token(span.token), limit);
        }
    }

    /// A run of tool calls in one row, its members one row each while the run
    /// shows them: a run that still goes and a run of one call draw their members
    /// alone, and every other run draws its summary row between two padding rows
    /// of its card first.
    ///
    /// A folded run keeps the diff of its newest edit inside the summary's card
    /// and the cards of its failed calls under it, so no line of the run hides
    /// while the run is folded.
    fn paint_group(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        group: &GroupRow<'_>,
        folds: &Folds,
        open: bool,
        shown: usize,
        styles: &Styles,
    ) {
        // The summary row comes when the run ends, and a lone call keeps its own.
        if group.going || group.calls == 1 {
            group.members.paint_members(rows, sink, styles, folds, open);
            return;
        }
        let summary = summary_styles(styles, group.failed > 0);
        let paints = &summary.paints;
        // An open run hangs its members from its summary card, so the card's
        // edge is the line they hang from rather than the bar.
        let edge = if open { RAIL } else { BAR };
        let mut counts = [0u8; DIGITS];
        let mut adds = [0u8; DIGITS];
        let mut removes = [0u8; DIGITS];
        let mut total = [0u8; DIGITS];
        let mut fails = [0u8; DIGITS];
        let added = digits(&mut adds, group.added);
        let removed = digits(&mut removes, group.removed);
        let lines = thousands(&mut total, group.lines);
        let failed = digits(&mut fails, u64::from(group.failed));
        let marker = if open { "▾" } else { "▸" };
        let mut right = 2 + columns(marker);
        if group.edits {
            // The space before each of the two signs and their number.
            right += 2 + columns(added) + 2 + columns(removed);
            if let Some(language) = group.language {
                right += columns(language);
            }
        } else {
            right += columns(lines) + columns(" lines");
        }
        pad_edge(rows, sink, &summary, edge);
        if let Some(y) = card_edge(rows, sink, &summary, edge) {
            let width = rows.width;
            let limit = width.saturating_sub(right).saturating_sub(1);
            let glyph = if group.failed > 0 { "✗" } else { "✓" };
            let mut x = sink.put_until(y, 2, glyph, paints.glyph, limit);
            x = sink.put_until(y, x, " ", paints.text, limit);
            for (index, counted) in group.names.iter().enumerate() {
                if index > 0 {
                    x = sink.put_until(y, x, " ", paints.text, limit);
                    x = sink.put_until(y, x, "·", paints.dim, limit);
                    x = sink.put_until(y, x, " ", paints.text, limit);
                }
                x = sink.put_until(y, x, &counted.name, paints.name, limit);
                x = sink.put_until(y, x, " ", paints.text, limit);
                let count = digits(&mut counts, u64::from(counted.count));
                x = sink.put_until(y, x, count, paints.text, limit);
                // The reads keep their wording: a count of files.
                if counted.name == "read" {
                    let files = if counted.count == 1 {
                        " file"
                    } else {
                        " files"
                    };
                    x = sink.put_until(y, x, files, paints.text, limit);
                }
            }
            if group.failed > 0 {
                let chip = x.saturating_add(1);
                x = sink.put_until(y, chip, " ", paints.chip, limit);
                x = sink.put_until(y, x, failed, paints.chip, limit);
                sink.put_until(y, x, " failed ", paints.chip, limit);
            }
            let mut at = width.saturating_sub(right);
            if group.edits {
                if let Some(language) = group.language {
                    at = sink.put_until(y, at, language, paints.dim, width);
                }
                at = sink.put_until(y, at, " ", paints.add, width);
                at = sink.put_until(y, at, "+", paints.add, width);
                at = sink.put_until(y, at, added, paints.add, width);
                at = sink.put_until(y, at, " ", paints.delete, width);
                at = sink.put_until(y, at, "-", paints.delete, width);
                at = sink.put_until(y, at, removed, paints.delete, width);
            } else {
                at = sink.put_until(y, at, lines, paints.dim, width);
                at = sink.put_until(y, at, " lines", paints.dim, width);
            }
            sink.put_until(y, at.saturating_add(2), marker, paints.dim, width);
        }
        if !open {
            // The newest edit of the run keeps its diff under the summary row,
            // in the summary's card.
            if let Some(edit) = group.newest {
                paint_diff(rows, sink, &summary, shown, &edit.diff);
            }
        }
        pad_edge(rows, sink, &summary, edge);
        if !open {
            // The failed calls of the run keep their cards under the summary
            // row, so no error hides while the run is folded.
            group.members.paint_failed(rows, sink, styles, folds);
            return;
        }
        rail_air(rows, sink, styles.paints.bar);
        group.members.paint_members(rows, sink, styles, folds, open);
    }

    /// The dim rule a run ends with, under a blank row: six dashes, then its
    /// time, tokens and cost.
    fn paint_rule(rows: &mut Rows, sink: &mut dyn Sink, usage: Usage, styles: &Styles) {
        rows.air(sink);
        let Some(y) = rows.line(sink) else {
            return;
        };
        let dim = styles.paints.dim;
        let mut secs = [0u8; DIGITS];
        let mut toks = [0u8; DIGITS];
        let mut money = [0u8; DIGITS];
        let seconds = digits(&mut secs, u64::from(usage.seconds));
        let tokens = tokens(&mut toks, usage.tokens);
        let cost = cost(&mut money, usage.cents);
        let mut x = sink.put(y, 0, "──────", dim);
        x = sink.put(y, x, " ", dim);
        x = sink.put(y, x, seconds, dim);
        x = sink.put(y, x, "s · ", dim);
        x = sink.put(y, x, tokens, dim);
        x = sink.put(y, x, " tokens · ", dim);
        sink.put(y, x, cost, dim);
    }

    /// The row above the text box that holds the message which steers the run,
    /// on the panel's fill.
    fn paint_steer(
        rows: &mut Rows,
        sink: &mut dyn Sink,
        text: &str,
        queued: bool,
        styles: &Styles,
    ) {
        let paints = &styles.paints;
        let Some(y) = rows.line(sink) else {
            return;
        };
        if let Some(fill) = paints.fill {
            sink.fill(y, fill);
        }
        let width = rows.width;
        let (label, hint) = if queued {
            ("queued", "next turn · backspace takes it back")
        } else {
            ("steer", "lands after this tool · backspace takes it back")
        };
        let mut x = sink.put(y, 0, "↳", paints.accent);
        x = sink.put(y, x.saturating_add(1), label, paints.name);
        x = sink.put(y, x, "  ", paints.text);
        x = sink.put_until(y, x, text, paints.text, width);
        x = sink.put_until(y, x, "  ", paints.text, width);
        sink.put_until(y, x, hint, paints.dim, width);
    }

    /// The row that says how many lines the viewport is behind, centered on the
    /// panel's fill.
    fn paint_new_lines(rows: &mut Rows, sink: &mut dyn Sink, count: u64, styles: &Styles) {
        let paints = &styles.paints;
        let Some(y) = rows.line(sink) else {
            return;
        };
        if let Some(fill) = paints.fill {
            sink.fill(y, fill);
        }
        let width = rows.width;
        let mut buf = [0u8; DIGITS];
        let count = digits(&mut buf, count);
        let total = 2 + columns(count) + 1 + columns("new lines") + 2 + columns("end jumps down");
        let mut x = width.saturating_sub(total) / 2;
        x = sink.put(y, x, "↓", paints.accent);
        x = sink.put(y, x, " ", paints.text);
        x = sink.put(y, x, count, paints.name);
        x = sink.put(y, x, " ", paints.text);
        x = sink.put(y, x, "new lines", paints.name);
        x = sink.put(y, x, "  ", paints.text);
        sink.put_until(y, x, "end jumps down", paints.dim, width);
    }

    /// The rows `text` wraps into at `width` columns: a slice of it per row,
    /// split at newlines, at the last space that fits and before a glyph that
    /// does not fit. A word longer than a row splits inside it, and the space
    /// a row breaks at is not drawn at the start of the next one. An empty text
    /// is one empty row.
    pub(crate) struct Wrap<'a> {
        text: &'a str,
        at: usize,
        width: usize,
        first: bool,
    }

    impl<'a> Wrap<'a> {
        /// The rows of `text` at `width` columns.
        pub(crate) fn new(text: &'a str, width: u16) -> Self {
            Wrap {
                text,
                at: 0,
                width: usize::from(width.max(1)),
                first: true,
            }
        }
    }

    impl<'a> Iterator for Wrap<'a> {
        type Item = &'a str;

        fn next(&mut self) -> Option<&'a str> {
            if self.at >= self.text.len() {
                let empty = self.first;
                self.first = false;
                return empty.then_some("");
            }
            self.first = false;
            let rest = &self.text[self.at..];
            if rest.starts_with('\n') {
                self.at += 1;
                return Some("");
            }
            let line = match rest.find('\n') {
                Some(end) => &rest[..end],
                None => rest,
            };
            let mut used = 0;
            let mut end = line.len();
            let mut space = 0;
            for (at, c) in line.char_indices() {
                if at > 0 && used + cells(c) > self.width {
                    end = at;
                    break;
                }
                used += cells(c);
                if c == ' ' {
                    space = at + 1;
                }
            }
            if end == line.len() {
                self.at += end;
                if self.text[self.at..].starts_with('\n') {
                    self.at += 1;
                }
                return Some(line);
            }
            let at_space = space > 1;
            let end = if at_space { space - 1 } else { end };
            self.at += end;
            if at_space {
                while self.text[self.at..].starts_with(' ') {
                    self.at += 1;
                }
            }
            Some(&line[..end])
        }
    }

    /// The digits of `n` in a buffer, for a row that draws a number.
    const DIGITS: usize = 24;

    /// The bytes the upper case of one letter takes at most: the three letters
    /// a one-to-many mapping writes, four bytes each.
    const CASE: usize = 12;

    /// The cells one char takes in a row: a tab is one space and the other
    /// control characters take none, the way the grid writes them.
    fn cells(c: char) -> usize {
        if c.is_control() {
            usize::from(c == '\t')
        } else {
            UnicodeWidthChar::width(c).unwrap_or(0)
        }
    }

    /// The columns `text` takes.
    fn columns(text: &str) -> u16 {
        u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
    }

    /// The digits of `n` at the end of `buf`.
    fn digits(buf: &mut [u8; DIGITS], mut n: u64) -> &str {
        let mut at = buf.len();
        loop {
            at -= 1;
            buf[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        std::str::from_utf8(&buf[at..]).unwrap_or_default()
    }

    /// The first letter of `name` upper-cased into `buf`, and the rest of
    /// `name`: a plain row's name draws as these two pieces, so no row builds a
    /// string.
    fn capital<'a>(buf: &'a mut [u8; CASE], name: &'a str) -> (&'a str, &'a str) {
        let mut rest = name.chars();
        let Some(first) = rest.next() else {
            return ("", name);
        };
        let mut len = 0;
        for letter in first.to_uppercase() {
            len += letter.encode_utf8(&mut buf[len..]).len();
        }
        (
            std::str::from_utf8(&buf[..len]).unwrap_or(""),
            rest.as_str(),
        )
    }

    /// The digits of `n` at the end of `buf`, right justified in `width`
    /// columns, for a diff's line number.
    fn right_in(buf: &mut [u8; DIGITS], mut n: u64, width: usize) -> &str {
        let width = width.clamp(1, buf.len());
        let start = buf.len() - width;
        buf[start..].fill(b' ');
        let mut at = buf.len();
        loop {
            at -= 1;
            if at < start {
                break;
            }
            buf[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        std::str::from_utf8(&buf[start..]).unwrap_or_default()
    }

    /// The digits of `n` with a comma between the thousands: `1,480`.
    fn thousands(buf: &mut [u8; DIGITS], mut n: u64) -> &str {
        let mut at = buf.len();
        let mut group = 0;
        loop {
            at -= 1;
            buf[at] = b'0' + (n % 10) as u8;
            n /= 10;
            group += 1;
            if n == 0 {
                break;
            }
            if group == 3 {
                group = 0;
                at -= 1;
                buf[at] = b',';
            }
        }
        std::str::from_utf8(&buf[at..]).unwrap_or_default()
    }

    /// The token count a rule writes: `842`, `4.1k`, `128k`.
    fn tokens(buf: &mut [u8; DIGITS], n: u64) -> &str {
        if n < 1000 {
            return digits(buf, n);
        }
        let mut whole = n / 1000;
        let tenth = (n % 1000) / 100;
        let mut at = buf.len();
        at -= 1;
        buf[at] = b'k';
        if whole < 100 {
            at -= 1;
            buf[at] = b'0' + tenth as u8;
            at -= 1;
            buf[at] = b'.';
        }
        loop {
            at -= 1;
            buf[at] = b'0' + (whole % 10) as u8;
            whole /= 10;
            if whole == 0 {
                break;
            }
        }
        std::str::from_utf8(&buf[at..]).unwrap_or_default()
    }

    /// The cost a rule writes, from cents: `$0.03`, `$1.20`.
    fn cost(buf: &mut [u8; DIGITS], cents: u32) -> &str {
        let mut at = buf.len();
        at -= 1;
        buf[at] = b'0' + (cents % 10) as u8;
        at -= 1;
        buf[at] = b'0' + (cents / 10 % 10) as u8;
        at -= 1;
        buf[at] = b'.';
        let mut whole = u64::from(cents / 100);
        loop {
            at -= 1;
            buf[at] = b'0' + (whole % 10) as u8;
            whole /= 10;
            if whole == 0 {
                break;
            }
        }
        at -= 1;
        buf[at] = b'$';
        std::str::from_utf8(&buf[at..]).unwrap_or_default()
    }

    #[cfg(test)]
    mod tests {
        use std::collections::VecDeque;
        use std::fmt::Write as _;
        use std::time::Instant;

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use reagent_ai::{AssistantMessage, Content, Message, Text, ToolCall, ToolResult};

        use crate::client::{Frame, Initialized, RunEnd};
        use crate::theme::{Depth, Mode, Rgb};

        use super::super::SPANS;
        use super::*;
        use crate::app::fold::DIFF_FOLD_LINES;
        use crate::app::{Group, Item, MARGIN, Tail, Thinking, Tool};

        /// The columns and the rows of the test terminal.
        const COLUMNS: u16 = 120;
        const ROWS: u16 = 40;

        /// The columns of the narrow terminal: a user turn wraps at 34 columns.
        const NARROW: u16 = 40;

        /// The rows a card takes above its body: the blank row before it, its
        /// padding row, its title and its padding row.
        const CARD_ROWS: usize = 4;

        /// The background a dark terminal answers with.
        const DARK: Rgb = Rgb::new(0x14, 0x14, 0x14);

        /// The diff of the edit the rows hold, longer than one fold: the line
        /// number, what it did and its text.
        const EDIT: [(u32, DiffKind, &str); 9] = [
            (
                636,
                DiffKind::Context,
                "let provider = home.script(\"one\", REPLY);",
            ),
            (
                637,
                DiffKind::Delete,
                "let mut tui = Tui::start(&home, ARGS);",
            ),
            (637, DiffKind::Add, "let mut start = Duration::MAX;"),
            (638, DiffKind::Add, "for round in 0..STARTS {"),
            (639, DiffKind::Add, "let began = Instant::now();"),
            (640, DiffKind::Add, "let sample = start();"),
            (641, DiffKind::Add, "start = start.min(sample);"),
            (642, DiffKind::Add, "}"),
            (643, DiffKind::Add, "assert!(start <= BUDGET);"),
        ];

        /// The result of the edit at the right edge of its row.
        const EDIT_RIGHT: [Span<'static>; 3] = [
            Span {
                text: "rust",
                role: SpanRole::Dim,
            },
            Span {
                text: "  +14",
                role: SpanRole::Add,
            },
            Span {
                text: " -3",
                role: SpanRole::Delete,
            },
        ];

        /// An app in full screen at 120 columns, the sidebar off so the main
        /// area is the whole terminal, in rich mode when `rich` is set.
        fn app(rich: bool) -> App {
            let mut app = App::new(&Initialized::default(), (COLUMNS, ROWS), true);
            app.sidebar_on = Some(false);
            app.reflow();
            app.look.color = true;
            if rich {
                app.look.depth = Depth::TrueColor;
                app.restyle(Mode::Rich {
                    background: DARK,
                    dark: true,
                });
            }
            app
        }

        /// The text of one drawn row of the screen from column 0, trailing blanks
        /// trimmed. The text box, the status line and the header row keep the
        /// whole width or the column the box draws its bar in.
        fn line(app: &App, y: u16) -> String {
            let grid = app.screen.back();
            let mut text = String::new();
            for x in 0..grid.width() {
                if let Some(c) = grid.cell(x, y).char() {
                    text.push(c);
                }
            }
            while text.ends_with(' ') {
                text.pop();
            }
            text
        }

        /// An app at `columns` columns in full screen, the sidebar off so the
        /// main area is the whole terminal, in rich mode.
        fn at_width(columns: u16) -> App {
            let mut app = App::new(&Initialized::default(), (columns, ROWS), true);
            app.sidebar_on = Some(false);
            app.reflow();
            app.look.color = true;
            app.look.depth = Depth::TrueColor;
            app.restyle(Mode::Rich {
                background: DARK,
                dark: true,
            });
            app
        }

        /// The text of one drawn row from the transcript's left margin, trailing
        /// blanks trimmed: the rows of the transcript start there.
        fn row(app: &App, y: u16) -> String {
            line(app, y).chars().skip(usize::from(MARGIN)).collect()
        }

        /// The style id of one cell of a row, its column counted from the left
        /// margin.
        fn style(app: &App, x: u16, y: u16) -> u32 {
            app.screen.back().cell(x + MARGIN, y).style()
        }

        /// True when `id` is a role and an emphasis the theme interned, so the
        /// cell draws no fill of its own.
        fn themed(app: &App, id: u32) -> bool {
            Role::ALL.into_iter().any(|role| {
                Emphasis::ALL
                    .into_iter()
                    .any(|emphasis| app.style(role, emphasis) == id)
            })
        }

        /// The id of a role drawn over the tint of `fill`.
        fn over(app: &mut App, role: Role, emphasis: Emphasis, fill: Role) -> u32 {
            let mut style = app.look.theme.styled(role, emphasis);
            style.bg = app.look.theme.fill(fill);
            app.screen.intern(style)
        }

        /// One entry of a block.
        fn entry(id: u64, block: Block<'_>) -> Entry<'_> {
            Entry { id, block }
        }

        /// The text of every drawn row of the screen, top down.
        fn screen_rows(app: &App) -> Vec<String> {
            (0..app.screen.rows()).map(|y| row(app, y)).collect()
        }

        /// The row the screen shows `needle` on, when it shows it at all.
        fn at(rows: &[String], needle: &str) -> Option<usize> {
            rows.iter().position(|row| row.contains(needle))
        }

        /// One call of a run, as the transcript holds it after its result: done,
        /// with an output of `lines` lines.
        fn answered(id: &str, name: &str, arguments: &str, lines: usize) -> Tool {
            let mut tool = Tool::new(id, name);
            tool.set_call(name, arguments, COLUMNS);
            tool.set_output(&"x\n".repeat(lines), COLUMNS);
            tool.finish();
            tool
        }

        /// One call of a run, with a one-line output.
        fn call(id: &str, name: &str, arguments: &str) -> Tool {
            answered(id, name, arguments, 1)
        }

        /// A call whose result was an error.
        fn failed(id: &str, name: &str, arguments: &str) -> Tool {
            let mut tool = Tool::new(id, name);
            tool.set_call(name, arguments, COLUMNS);
            tool.set_result(9, Some("no such file"), COLUMNS);
            tool.finish();
            tool
        }

        /// A finished edit of `path` that adds `added` lines and removes
        /// `removed`.
        fn edited(path: &str, added: usize, removed: usize) -> Tool {
            let old = "old\\n".repeat(removed);
            let new = "new\\n".repeat(added);
            let arguments =
                format!(r#"{{"path":"{path}","old_string":"{old}","new_string":"{new}"}}"#);
            let mut tool = Tool::new("edit", "edit");
            tool.set_call("edit", &arguments, COLUMNS);
            tool.set_result(2, None, COLUMNS);
            tool.finish();
            tool
        }

        /// A group of three reads and two greps, the calls a turn's reads and
        /// searches group into.
        fn group(fold: u64) -> Group {
            Group::new(
                vec![
                    answered("1", "read", r#"{"path":"src/app/mod.rs"}"#, 480),
                    answered("2", "read", r#"{"path":"src/app/fold.rs"}"#, 500),
                    answered("3", "read", r#"{"path":"src/app/full.rs"}"#, 500),
                    answered("4", "grep", r#"{"pattern":"fn paint"}"#, 0),
                    answered("5", "grep", r#"{"pattern":"members"}"#, 0),
                ],
                fold,
            )
        }

        /// The column a row holds `needle` at, counted from the left margin.
        fn column(line: &str, needle: &str) -> u16 {
            let at = line.find(needle).expect("the row holds it");
            u16::try_from(line[..at].chars().count()).expect("a column")
        }

        /// Paints the summary row of `group` at the top of the screen, drawn as
        /// the app's fold state has it.
        fn paint_summary(app: &mut App, group: &Group) {
            let folds = app.folds.clone();
            let entries = [entry(group.fold, Block::Group(group.row()))];
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
        }

        /// A finished edit and the lines of its diff, which its row borrows.
        struct Edit {
            /// The lines of the diff under its row.
            diff: Vec<OwnedDiff>,
        }

        impl Edit {
            /// The edit the tests draw: a change of fourteen lines for three.
            fn new() -> Self {
                let diff = EDIT
                    .into_iter()
                    .map(|(number, kind, text)| OwnedDiff {
                        number,
                        kind,
                        text: text.to_owned(),
                        tokens: Vec::new(),
                    })
                    .collect();
                Edit { diff }
            }

            /// The row of the edit, the diff under it.
            fn row(&self) -> ToolRow<'_> {
                ToolRow {
                    name: "edit",
                    target: "crates/cli/tests/tui_footprint.rs",
                    state: State::Done,
                    right: &EDIT_RIGHT,
                    diff: &self.diff,
                    error: None,
                    output: None,
                    earlier: 0,
                }
            }
        }

        #[test]
        fn transcript_row_user_turn_is_accent_bold_under_a_blank_row() {
            let mut app = app(true);
            let entries = [entry(
                1,
                Block::User {
                    text: "make it stable",
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 3, "a blank row, the turn and a blank row");
            assert!(row(&app, 0).is_empty(), "one blank row before the turn");
            assert_eq!(row(&app, 1), "› make it stable");
            assert!(row(&app, 2).is_empty(), "one blank row after the turn");
            assert_eq!(
                style(&app, 0, 1),
                app.style(Role::Accent, Emphasis::Bold),
                "the `›` is the accent, bold"
            );
            assert_eq!(
                style(&app, 2, 1),
                app.style(Role::Text, Emphasis::Bold),
                "the text is bold"
            );
            for x in 0..COLUMNS {
                assert!(
                    themed(&app, style(&app, x, 1)),
                    "a user turn draws no fill, cell {x} has one"
                );
            }
        }

        #[test]
        fn transcript_row_user_turn_wraps_under_a_two_column_indent() {
            let mut app = app(true);
            let text = "the start test fails right after a release build, so it needs more than one sample of the first frame to pass on a cold cache";
            let entries = [entry(1, Block::User { text })];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 4, "two rows of text between blank rows");
            let second = row(&app, 1);
            let third = row(&app, 2);
            assert!(
                second.chars().count() <= usize::from(COLUMNS - 2 * MARGIN),
                "{second}"
            );
            assert!(
                second.ends_with("to pass on"),
                "the row breaks at a space: {second:?}"
            );
            assert!(third.starts_with("  "), "the wrap is indented: {third:?}");
            assert_eq!(style(&app, 2, 2), app.style(Role::Text, Emphasis::Bold));
        }

        #[test]
        fn transcript_row_user_turn_wraps_at_a_space() {
            let mut app = at_width(NARROW);
            let entries = [entry(
                1,
                Block::User {
                    text: "make the start test stable on a cold cache",
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 4, "two rows of text between blank rows");
            assert_eq!(row(&app, 1), "› make the start test stable on a");
            assert_eq!(row(&app, 2), "  cold cache", "no space at the row start");
        }

        #[test]
        fn transcript_row_user_turn_wraps_a_long_word_inside_it() {
            let mut app = at_width(NARROW);
            let entries = [entry(
                1,
                Block::User {
                    text: "open crates/tui/src/app/paint/transcript.rs now",
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 5, "three rows of text between blank rows");
            assert_eq!(row(&app, 1), "› open");
            assert_eq!(
                row(&app, 2),
                "  crates/tui/src/app/paint/transcrip",
                "a word longer than a row splits inside it"
            );
            assert_eq!(row(&app, 3), "  t.rs now");
        }

        #[test]
        fn transcript_row_user_turn_wraps_keeps_a_newline() {
            let mut app = at_width(NARROW);
            let entries = [entry(1, Block::User { text: "one\ntwo" })];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 4, "two rows of text between blank rows");
            assert_eq!(row(&app, 1), "› one");
            assert_eq!(row(&app, 2), "  two");
        }

        #[test]
        fn transcript_row_thinking_is_one_dim_italic_row() {
            let mut app = app(true);
            let mut folds = Folds::new();
            let id = folds.add();
            let entries = [entry(
                id,
                Block::Thinking {
                    seconds: 4,
                    text: "the first start pays for the linker",
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &folds);
            assert_eq!(drawn, 3, "a folded thought is one row between blank rows");
            assert_eq!(row(&app, 1), "thought for 4s  ▸");
            assert_eq!(
                style(&app, 0, 1),
                app.style(Role::Dim, Emphasis::DimItalic),
                "the row is dim italic"
            );
            folds.toggle(id);
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &folds);
            assert_eq!(
                drawn, 4,
                "an open thought shows its newest line between the row and the blank"
            );
            assert!(row(&app, 2).starts_with("the first start pays"));
        }

        #[test]
        fn thinking_row_has_a_blank_row_above_and_below() {
            let mut app = app(true);
            let entries = [entry(
                1,
                Block::Thinking {
                    seconds: 4,
                    text: "the first start pays for the linker",
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 3, "a blank row, the thought and a blank row");
            assert!(row(&app, 0).is_empty(), "one blank row before the thought");
            assert_eq!(row(&app, 1), "thought for 4s  ▸");
            assert!(row(&app, 2).is_empty(), "one blank row after the thought");
        }

        #[test]
        fn thinking_under_a_second_with_no_text_draws_nothing() {
            let mut app = app(true);
            let entries = [entry(
                1,
                Block::Thinking {
                    seconds: 0,
                    text: "",
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 0, "an empty thought under a second draws no row");
            assert!(
                (0..ROWS).all(|y| row(&app, y).is_empty()),
                "it leaves no blank row either"
            );
        }

        #[test]
        fn thinking_under_a_second_reads_thought() {
            let mut app = app(true);
            let entries = [entry(
                1,
                Block::Thinking {
                    seconds: 0,
                    text: "a quick line",
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 3, "a blank row, the thought and a blank row");
            assert_eq!(row(&app, 1), "thought  ▸", "no time under a second");
        }

        #[test]
        fn tool_row_puts_the_result_at_the_right_edge() {
            let mut app = app(true);
            let edit = Edit::new();
            let entries = [entry(1, Block::Tool(edit.row()))];
            let folds = Folds::new();
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
            let line = row(&app, 2);
            assert!(
                line.starts_with("▎ ← Edit crates/cli/tests/tui_footprint.rs"),
                "{line}"
            );
            assert!(line.ends_with("+14 -3  ▸"), "{line}");
            assert_eq!(
                line.chars().count(),
                COLUMNS as usize - usize::from(2 * MARGIN),
                "the result ends before the right margin: {line}"
            );
            assert_eq!(
                style(&app, 2, 2),
                over(&mut app, Role::Dim, Emphasis::Plain, Role::SurfaceFill),
                "the mark of the card is dim over its surface"
            );
            assert_eq!(
                style(&app, 9, 2),
                over(&mut app, Role::Text, Emphasis::Plain, Role::SurfaceFill),
                "the path is the text role"
            );
            assert_eq!(
                style(&app, COLUMNS - 2 * MARGIN - 1, 2),
                over(&mut app, Role::Dim, Emphasis::Plain, Role::SurfaceFill),
                "the fold marker is dim"
            );
        }

        #[test]
        fn tool_row_shows_the_live_duration_while_it_runs() {
            let mut app = app(true);
            let tool = ToolRow {
                name: "edit",
                target: "cargo nextest run --release -p reagent --test tui_footprint",
                state: State::Running,
                right: &[Span {
                    text: "41.2s",
                    role: SpanRole::Dim,
                }],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            };
            let entries = [entry(1, Block::Tool(tool))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let line = row(&app, 2);
            assert!(line.starts_with("▎ ← Edit cargo nextest run"), "{line}");
            assert!(line.ends_with("41.2s  ▸"), "{line}");
            assert_eq!(
                line.chars().count(),
                COLUMNS as usize - 2 * usize::from(MARGIN),
                "{line}"
            );
            assert_eq!(
                style(&app, 0, 2),
                over(&mut app, Role::Warn, Emphasis::Plain, Role::SurfaceFill),
                "a running card draws its bar in the warning role"
            );
        }

        #[test]
        fn tool_row_failed_shows_the_cross_and_the_message() {
            let mut app = app(true);
            let tool = ToolRow {
                name: "bash",
                target: "rg \"SETTLE\" crates/cli/tests/tui_footprint.rs",
                state: State::Failed,
                right: &[Span {
                    text: "no such file",
                    role: SpanRole::Dim,
                }],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            };
            let entries = [entry(1, Block::Tool(tool))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let line = row(&app, 2);
            assert!(line.starts_with("▎ ✗ $ rg \"SETTLE\""), "{line}");
            assert!(
                !line.contains(" error "),
                "the cross says it without a chip: {line}"
            );
            assert!(line.ends_with("no such file  ▸"), "{line}");
            assert_eq!(
                style(&app, 2, 2),
                over(&mut app, Role::Error, Emphasis::Plain, Role::ErrFill),
                "the cross is the error role"
            );
        }

        /// Full screen keeps a margin of two blank columns at each side of the
        /// transcript: the text starts after the left one, and a row's tint
        /// covers both.
        #[test]
        fn a_tool_row_tints_the_margin_of_the_transcript() {
            let mut app = app(true);
            let tool = ToolRow {
                name: "edit",
                target: "crates/cli/tests/tui_footprint.rs",
                state: State::Done,
                right: &[Span {
                    text: "+14 -3",
                    role: SpanRole::Add,
                }],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            };
            app.paint_blocks(0, 0, ROWS, &[entry(1, Block::Tool(tool))], &Folds::new());
            let tint = over(&mut app, Role::Text, Emphasis::Plain, Role::SurfaceFill);
            for x in 0..MARGIN {
                assert_eq!(
                    app.screen.back().cell(x, 1).style(),
                    tint,
                    "the tint reaches column {x} of the left margin"
                );
                assert_eq!(
                    app.screen.back().cell(COLUMNS - 1 - x, 1).style(),
                    tint,
                    "the tint reaches column {} of the right margin",
                    COLUMNS - 1 - x
                );
            }
            assert_eq!(line(&app, 2).chars().count(), usize::from(COLUMNS - MARGIN));
            assert!(row(&app, 2).starts_with("▎ ← Edit"));
        }

        #[test]
        fn tool_row_tints_a_card_as_its_surface() {
            let mut app = app(true);
            assert_ne!(
                app.look.theme.fill(Role::SurfaceFill),
                Color::Default,
                "rich mode mixes a fill"
            );
            let tool = ToolRow {
                name: "edit",
                target: "a.rs",
                state: State::Done,
                right: &[Span {
                    text: "+14 -3",
                    role: SpanRole::Add,
                }],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            };
            let entries = [entry(1, Block::Tool(tool))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let want = over(&mut app, Role::Text, Emphasis::Plain, Role::SurfaceFill);
            assert_eq!(
                style(&app, 60, 2),
                want,
                "the whole title row is over the tint"
            );
            assert_eq!(
                style(&app, COLUMNS - 2 * MARGIN - 1, 2),
                over(&mut app, Role::Dim, Emphasis::Plain, Role::SurfaceFill),
                "the text of the row sits over the same tint"
            );
        }

        #[test]
        fn tool_row_draws_no_fill_in_terminal_mode() {
            let mut app = app(false);
            assert_eq!(
                app.look.theme.fill(Role::OkFill),
                Color::Default,
                "terminal mode has no fills"
            );
            let edit = Edit::new();
            let entries = [entry(1, Block::Tool(edit.row()))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            for x in 0..COLUMNS {
                assert!(
                    themed(&app, style(&app, x, 1)),
                    "cell {x} is built off a role, so no cell draws a background"
                );
            }
        }

        /// A small tool's call draws as one plain row: the icon and the name
        /// upper-cased and dim, the target in the text role, the result at the
        /// right edge and no tint.
        #[test]
        fn plain_row_draws_a_read_with_its_icon_and_no_fill() {
            let mut app = app(true);
            let tool = ToolRow {
                name: "read",
                target: "crates/tui/src/panels/sessions.rs",
                state: State::Done,
                right: &[Span {
                    text: "212 lines",
                    role: SpanRole::Dim,
                }],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            };
            let entries = [entry(1, Block::Tool(tool))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let line = row(&app, 1);
            assert!(
                line.starts_with("→ Read crates/tui/src/panels/sessions.rs"),
                "{line}"
            );
            assert!(line.ends_with("212 lines  ▸"), "{line}");
            assert_eq!(
                line.chars().count(),
                COLUMNS as usize - 2 * usize::from(MARGIN),
                "{line}"
            );
            assert_eq!(
                style(&app, 0, 1),
                app.style(Role::Dim, Emphasis::Plain),
                "the icon is dim: {line}"
            );
            assert_eq!(
                style(&app, 2, 1),
                app.style(Role::Dim, Emphasis::Plain),
                "the name is dim: {line}"
            );
            assert_eq!(
                style(&app, column(&line, "crates/"), 1),
                app.style(Role::Text, Emphasis::Plain),
                "the target takes the text role: {line}"
            );
            assert_eq!(
                style(&app, column(&line, "212 lines"), 1),
                app.style(Role::Dim, Emphasis::Plain),
                "the result is dim: {line}"
            );
            for x in 0..COLUMNS {
                assert!(
                    themed(&app, style(&app, x, 1)),
                    "a plain row draws no fill, cell {x} has one"
                );
            }
        }

        #[test]
        fn plain_row_draws_a_grep_and_a_find_with_the_star() {
            let mut app = app(true);
            let entries = [
                entry(1, Block::Tool(bare("grep", "fn page"))),
                entry(2, Block::Tool(bare("find", "src/*.rs"))),
            ];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 5, "a blank row, two rows and a blank row");
            let grep = row(&app, 1);
            let find = row(&app, 3);
            assert!(
                grep.starts_with("✱ Grep \"fn page\""),
                "a grep takes the star: {grep}"
            );
            assert!(
                find.starts_with("✱ Find \"src/*.rs\""),
                "a find takes the star too: {find}"
            );
            assert_eq!(
                style(&app, 0, 1),
                app.style(Role::Dim, Emphasis::Plain),
                "the star is dim: {grep}"
            );
        }

        #[test]
        fn plain_row_draws_an_mcp_tool_with_the_diamond() {
            let mut app = app(true);
            let tool = ToolRow {
                name: "mcp__notes__search",
                target: "the page test",
                state: State::Done,
                right: &[Span {
                    text: "1 match",
                    role: SpanRole::Dim,
                }],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            };
            let entries = [entry(1, Block::Tool(tool))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let line = row(&app, 1);
            assert!(
                line.starts_with("◇ Mcp__notes__search the page test"),
                "{line}"
            );
            assert!(line.ends_with("1 match  ▸"), "{line}");
            assert_eq!(
                style(&app, 0, 1),
                app.style(Role::Dim, Emphasis::Plain),
                "the diamond is dim: {line}"
            );
            assert_eq!(
                style(&app, 2, 1),
                app.style(Role::Dim, Emphasis::Plain),
                "only the first letter is upper-cased: {line}"
            );
        }

        #[test]
        fn plain_row_leaves_a_bash_card_and_an_edit_alone() {
            let mut app = app(true);
            let edit = Edit::new();
            let bash = ToolRow {
                name: "bash",
                target: "cargo test",
                state: State::Done,
                right: &[Span {
                    text: "8 passed",
                    role: SpanRole::Add,
                }],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            };
            let entries = [
                entry(1, Block::Tool(bash)),
                entry(2, Block::Tool(edit.row())),
            ];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let line = row(&app, 2);
            assert!(line.starts_with("▎ $ cargo test"), "{line}");
            assert!(line.ends_with("8 passed  ▸"), "{line}");
            assert_eq!(
                style(&app, 0, 2),
                over(&mut app, Role::Line, Emphasis::Plain, Role::SurfaceFill),
                "a bash call is a card of the surface, not a plain row: {line}"
            );
            let line = row(&app, 6);
            assert!(
                line.starts_with("▎ ← Edit crates/cli/tests/tui_footprint.rs"),
                "{line}"
            );
            assert!(line.ends_with("+14 -3  ▸"), "{line}");
            assert_eq!(
                style(&app, 0, 6),
                over(&mut app, Role::Line, Emphasis::Plain, Role::SurfaceFill),
                "an edit is a card of the surface too: {line}"
            );
        }

        #[test]
        fn plain_row_of_a_failed_read_has_no_tint_and_opens_to_its_error() {
            let mut app = app(true);
            let tool = ToolRow {
                name: "read",
                target: "src/app/mod.rs",
                state: State::Failed,
                right: &[Span {
                    text: "no such file",
                    role: SpanRole::Text,
                }],
                diff: &[],
                error: Some("no such file or directory"),
                output: None,
                earlier: 0,
            };
            let mut folds = Folds::new();
            let id = folds.add();
            folds.toggle(id);
            let entries = [entry(id, Block::Tool(tool))];
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
            let line = row(&app, 1);
            assert!(line.starts_with("→ Read src/app/mod.rs"), "{line}");
            assert!(line.ends_with("no such file  ▾"), "{line}");
            assert!(
                !line.contains(" error "),
                "a plain row draws no chip: {line}"
            );
            assert_eq!(
                style(&app, column(&line, "no such file"), 1),
                app.style(Role::Error, Emphasis::Plain),
                "the result keeps the error role: {line}"
            );
            for x in 0..COLUMNS {
                assert!(
                    themed(&app, style(&app, x, 1)),
                    "a failed plain row draws no tint either, cell {x} has one"
                );
            }
            assert_eq!(
                row(&app, 2),
                "  no such file or directory",
                "an open plain row shows its error"
            );
            assert_eq!(
                style(&app, 2, 2),
                app.style(Role::Error, Emphasis::Plain),
                "the error under the row takes the error role"
            );
        }

        #[test]
        fn fold_group_of_three_reads_and_two_greps_is_one_row() {
            let mut app = app(true);
            let fold = app.folds.add();
            let drawn = app.paint_blocks(
                0,
                0,
                ROWS,
                &[entry(fold, Block::Group(group(fold).row()))],
                &Folds::new(),
            );
            assert_eq!(
                drawn, 5,
                "the summary row between two padding rows, between two blank rows"
            );
            let line = row(&app, 2);
            assert!(
                line.starts_with("\u{258e} ✓ read 3 files · grep 2"),
                "{line}"
            );
            assert!(line.ends_with("1,480 lines  ▸"), "{line}");
            assert_eq!(
                line.chars().count(),
                COLUMNS as usize - 2 * usize::from(MARGIN),
                "{line}"
            );
        }

        /// A group of only one kind of call leaves the other count out: no
        /// `grep 0`, and no `read 0 files`.
        #[test]
        fn fold_group_writes_only_the_counts_it_has() {
            let mut app = app(true);
            let fold = app.folds.add();
            let greps = Group::new(
                vec![
                    answered("1", "grep", r#"{"pattern":"fn paint"}"#, 20),
                    answered("2", "grep", r#"{"pattern":"members"}"#, 20),
                ],
                fold,
            );
            paint_summary(&mut app, &greps);
            let line = row(&app, 2);
            assert!(line.starts_with("\u{258e} ✓ grep 2"), "{line}");
            assert!(!line.contains("read"), "no read count of 0: {line}");
            app.screen.clear();
            let reads = Group::new(
                vec![
                    answered("1", "read", r#"{"path":"src/app/mod.rs"}"#, 20),
                    answered("2", "read", r#"{"path":"src/app/fold.rs"}"#, 20),
                ],
                fold,
            );
            paint_summary(&mut app, &reads);
            let line = row(&app, 2);
            assert!(line.starts_with("\u{258e} ✓ read 2 files"), "{line}");
            assert!(!line.contains("grep"), "no grep count of 0: {line}");
            assert!(!line.contains("·"), "no separator with one part: {line}");
        }

        /// The summary row names each tool of the run with its count, in the
        /// order the calls came in.
        #[test]
        fn summary_row_counts_each_tool_in_call_order() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    call("1", "bash", r#"{"command":"cargo test"}"#),
                    edited("src/a.rs", 1, 0),
                    call("2", "bash", r#"{"command":"cargo clippy"}"#),
                    edited("src/b.rs", 1, 0),
                ],
                fold,
            );
            paint_summary(&mut app, &run);
            let line = row(&app, 2);
            assert!(line.starts_with("\u{258e} ✓ bash 2 · edit 2"), "{line}");
            // The summary row is a one-row card: its text sits on the surface.
            let surface = Role::SurfaceFill;
            assert_eq!(
                style(&app, column(&line, "bash"), 2),
                over(&mut app, Role::Text, Emphasis::Bold, surface),
                "the name is bold: {line}"
            );
            assert_eq!(
                style(&app, column(&line, "2 "), 2),
                over(&mut app, Role::Text, Emphasis::Plain, surface),
                "the count is plain: {line}"
            );
            assert_eq!(
                style(&app, column(&line, "·"), 2),
                over(&mut app, Role::Dim, Emphasis::Plain, surface),
                "the separator is dim: {line}"
            );
        }

        /// A run with a failed call draws a cross, and the number that failed in
        /// the chip a failed call's row uses.
        #[test]
        fn summary_row_counts_failures_with_the_chip_and_a_cross() {
            let mut app = app(false);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    call("1", "bash", r#"{"command":"cargo test"}"#),
                    failed("2", "bash", r#"{"command":"cargo clippy"}"#),
                    edited("src/a.rs", 2, 1),
                ],
                fold,
            );
            paint_summary(&mut app, &run);
            let line = row(&app, 2);
            assert!(line.starts_with("\u{258e} ✗ bash 2 · edit 1"), "{line}");
            assert!(line.contains(" 1 failed"), "{line}");
            assert_eq!(
                style(&app, column(&line, "1 failed"), 2),
                app.style(Role::Error, Emphasis::Bold),
                "the count of failures is a chip: {line}"
            );
            assert_eq!(
                style(&app, column(&line, "✗"), 2),
                app.style(Role::Error, Emphasis::Plain),
                "the cross takes the error role: {line}"
            );
        }

        /// A run of a failed bash call, an edit and a bash call: the calls the
        /// tests below fold into one summary row.
        fn failed_run(fold: u64) -> Group {
            Group::new(
                vec![
                    failed(
                        "1",
                        "bash",
                        r#"{"command":"cargo nextest run -p reagent-tui"}"#,
                    ),
                    edited("src/app/mod.rs", 5, 1),
                    call("2", "bash", r#"{"command":"cargo fmt"}"#),
                ],
                fold,
            )
        }

        /// A folded run draws the card of its failed call right under its
        /// summary row, red with the error it ended with, over the lines of its
        /// newest edit.
        #[test]
        fn folded_failed_card_draws_under_the_summary_row() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.pending.push(Item::Group(Box::new(failed_run(fold))));
            app.commit();
            app.paint();
            let rows = screen_rows(&app);
            let top = at(&rows, "✗ bash 2 · edit 1")
                .unwrap_or_else(|| panic!("the summary row draws:\n{}", rows.join("\n")));
            let line = &rows[top];
            assert!(line.contains(" 1 failed"), "the chip counts it: {line}");
            let title = at(&rows, "✗ $ cargo nextest run -p reagent-tui")
                .unwrap_or_else(|| panic!("the failed card draws:\n{}", rows.join("\n")));
            assert_eq!(
                title,
                top + 8,
                "the card's title sits under the summary row, the newest edit's\
                 lines, the padding row under them and a blank row:\n{}",
                rows.join("\n")
            );
            assert_eq!(rows[top + 7], "▎", "a padding row sits over the title");
            assert_eq!(rows[top + 9], "▎", "a padding row sits under the title");
            assert_eq!(
                rows[top + 10],
                "▎ no such file",
                "the body is the error the call failed with"
            );
            assert_eq!(rows[top + 11], "▎", "a padding row sits under the body");
            let bar = over(&mut app, Role::Error, Emphasis::Plain, Role::ErrFill);
            let fill = app.style(Role::ErrFill, Emphasis::Plain);
            for y in (top + 7)..=(top + 11) {
                let y = u16::try_from(y).expect("a row");
                assert_eq!(
                    style(&app, 0, y),
                    bar,
                    "row {y} has a bar in the error role"
                );
                assert_eq!(style(&app, 1, y), fill, "row {y} sits on the error tint");
            }
            let more = at(&rows, "more lines")
                .unwrap_or_else(|| panic!("the newest edit keeps its lines:\n{}", rows.join("\n")));
            assert!(
                more > top,
                "the newest edit draws under the summary row:\n{}",
                rows.join("\n")
            );
            assert_eq!(
                app.screen.transcript().ring_len(),
                15,
                "the ring counts a blank row, the summary's padding row, its row, the\
                 newest edit's four, its padding row, the card's six rows and a\
                 blank row:\n{}",
                rows.join("\n")
            );
        }

        /// The live area counts the rows of the cards it draws under a folded
        /// summary row, so a frame lays the transcript out over them.
        #[test]
        fn folded_failed_card_counts_its_rows_in_the_live_area() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.live.push(Item::Group(Box::new(failed_run(fold))));
            app.screen.clear();
            let drawn = app.paint_live(0, ROWS, 0);
            assert_eq!(
                drawn, 15,
                "a blank row, the summary's padding row, its row, the newest edit's\
                 four, its padding row, the card's six rows and a blank row"
            );
            assert_eq!(app.live_rows(), drawn, "the count is the rows it draws");
            let rows = screen_rows(&app);
            assert_eq!(
                at(&rows, "▎ ✗ $ cargo nextest run -p reagent-tui"),
                Some(10),
                "the card draws under the summary row:\n{}",
                rows.join("\n")
            );
        }

        /// An open run draws its calls once each, in their order: the card of a
        /// failed call comes with the call's member row, and not again under the
        /// summary row.
        #[test]
        fn folded_failed_card_is_not_drawn_twice_when_the_run_opens() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.folds.toggle(fold);
            let run = failed_run(fold);
            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let cards = rows
                .iter()
                .filter(|line| {
                    line.starts_with("├───\u{258e} ✗ $ cargo nextest run -p reagent-tui")
                })
                .count();
            assert_eq!(cards, 1, "the failed card draws once:\n{}", rows.join("\n"));
            let bash = at(&rows, "$ cargo nextest run -p reagent-tui").expect("the failed call");
            let edit = at(&rows, "← Edit src/app/mod.rs").expect("the edit");
            let fmt = at(&rows, "$ cargo fmt").expect("the second bash call");
            assert!(
                bash < edit && edit < fmt,
                "the members draw in the order they came in:\n{}",
                rows.join("\n")
            );
        }

        /// An open call in an open run keeps a blank row under what it shows,
        /// so the next member's row does not sit on its last line.
        #[test]
        fn open_call_in_an_open_run_keeps_a_blank_row_under_its_output() {
            let mut app = app(true);
            let fold = app.folds.add();
            let mut ask = answered(
                "1",
                "ask",
                r#"{"questions":[{"q":"Which language?","options":[{"label":"Rust"},{"label":"Go"}]}]}"#,
                3,
            );
            ask.fold = app.folds.add();
            let mut read = call("2", "read", r#"{"path":"src/x.rs"}"#);
            read.fold = app.folds.add();
            app.folds.toggle(fold);
            app.folds.toggle(ask.fold);
            let run = Group::new(vec![ask, read], fold);

            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let read = at(&rows, "src/x.rs").expect("the read's row");
            assert_eq!(
                rows[read - 1],
                "\u{2502}",
                "a blank row above the read:\n{shown}"
            );
            assert!(rows[read - 2].ends_with('x'), "the ask's output:\n{shown}");
        }

        /// An open run keeps a blank row with no fill between its summary card
        /// and a first member that draws no card, which takes no air itself.
        #[test]
        fn open_run_keeps_a_blank_row_above_a_plain_first_member() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    call("1", "read", r#"{"path":"src/x.rs"}"#),
                    call("2", "bash", r#"{"command":"ls"}"#),
                ],
                fold,
            );
            app.folds.toggle(fold);
            let surface = app.style(Role::SurfaceFill, Emphasis::Plain);
            let last = app.screen.columns() - MARGIN - 1;

            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let summary = at(&rows, "\u{2713} read 1").expect("the summary row");
            let read = at(&rows, "src/x.rs").expect("the read's row");
            assert_eq!(read, summary + 3, "pad, then a blank row:\n{shown}");
            assert_eq!(rows[read - 1], "\u{2502}", "{shown}");
            let pad = u16::try_from(summary + 1).expect("a row");
            assert_eq!(style(&app, last, pad), surface, "the pad:\n{shown}");
            let gap = u16::try_from(read - 1).expect("a row");
            assert_ne!(style(&app, last, gap), surface, "the gap:\n{shown}");
        }

        /// An open run hangs its members from a tree line: the summary's border
        /// is `│` in the middle of column 0, the line runs down the gaps, each
        /// member's first row takes `├───` (the last `└───`), and the members
        /// draw four columns in, the cards' fill and bar with them. A folded
        /// run keeps the card's `▎`.
        #[test]
        fn open_run_hangs_its_members_from_a_tree_line() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    edited("src/a.rs", 0, 0),
                    call("2", "bash", r#"{"command":"cargo test"}"#),
                    call("3", "read", r#"{"path":"src/x.rs"}"#),
                ],
                fold,
            );
            let surface = app.style(Role::SurfaceFill, Emphasis::Plain);
            let last = app.screen.columns() - MARGIN - 1;

            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let summary = at(&rows, "✓ edit 1").expect("the folded summary row");
            assert!(
                rows[summary].starts_with("\u{258e} ✓"),
                "a folded run keeps the card's bar:\n{}",
                rows.join("\n")
            );

            app.screen.clear();
            app.folds.toggle(fold);
            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let summary = at(&rows, "✓ edit 1").expect("the summary row");
            assert!(rows[summary].starts_with("│ ✓ edit 1"), "{shown}");
            assert_eq!(rows[summary - 1], "│", "{shown}");
            assert_eq!(rows[summary + 1], "│", "{shown}");
            let edit = at(&rows, "Edit src/a.rs").expect("the edit card's title");
            assert!(
                rows[edit].starts_with("├───\u{258e} ← Edit src/a.rs"),
                "{shown}"
            );
            assert_eq!(
                rows[edit - 1],
                "│   \u{258e}",
                "the edit card's pad:\n{shown}"
            );
            let bash = at(&rows, "$ cargo test").expect("the bash card's title");
            assert!(
                rows[bash].starts_with("├───\u{258e} $ cargo test"),
                "{shown}"
            );
            assert_eq!(rows[bash - 2], "│", "the gap above the bash card:\n{shown}");
            assert!(
                rows[bash + 2].starts_with("│   \u{258e} x"),
                "the bash card's body:\n{shown}"
            );
            let read = at(&rows, "src/x.rs").expect("the read's row");
            assert!(rows[read].starts_with("└───"), "{shown}");
            assert!(
                rows[read + 1..].iter().all(|row| !row.starts_with('│')),
                "the line ends at the last member:\n{shown}"
            );

            let y = u16::try_from(bash).expect("a row");
            assert_eq!(style(&app, last, y), surface, "the card's fill:\n{shown}");
            assert_eq!(style(&app, 5, y), surface, "the card starts at 4:\n{shown}");
            assert_ne!(style(&app, 2, y), surface, "the lead has no fill:\n{shown}");
            let gap = u16::try_from(bash - 2).expect("a row");
            assert_ne!(
                style(&app, 0, gap),
                surface,
                "the gap has no fill:\n{shown}"
            );
            let top = u16::try_from(summary - 1).expect("a row");
            assert_eq!(
                style(&app, last, top),
                surface,
                "the summary's pad:\n{shown}"
            );
        }

        /// The padding rows of a card in an open run, which hold no text but the
        /// bar, keep the card's fill to the right edge, as its title and body
        /// rows do.
        #[test]
        fn pad_rows_of_an_open_runs_cards_keep_their_fill() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    edited("src/a.rs", 0, 0),
                    call("2", "bash", r#"{"command":"cargo test"}"#),
                ],
                fold,
            );
            let surface = app.style(Role::SurfaceFill, Emphasis::Plain);
            let last = app.screen.columns() - MARGIN - 1;
            app.folds.toggle(fold);
            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let edit = at(&rows, "Edit src/a.rs").expect("the edit card's title");
            let bash = at(&rows, "$ cargo test").expect("the bash card's title");
            for (pad, what) in [
                (edit - 1, "above the edit title"),
                (edit + 1, "under the edit title"),
                (bash - 1, "above the bash title"),
                (bash + 1, "under the bash title"),
            ] {
                let y = u16::try_from(pad).expect("a row");
                assert_eq!(style(&app, 5, y), surface, "the pad {what}:\n{shown}");
                assert_eq!(style(&app, last, y), surface, "the pad {what}:\n{shown}");
            }
        }

        /// A run's summary row sits between two padding rows of the surface, a
        /// card of an open run sits one blank row with no fill away from the
        /// member before and after it, and a card with no body line is its
        /// title between two padding rows.
        #[test]
        fn summary_row_pads_and_run_cards_space() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    edited("src/a.rs", 0, 0),
                    edited("src/b.rs", 1, 1),
                    call("3", "bash", r#"{"command":"cargo test"}"#),
                ],
                fold,
            );
            let surface = app.style(Role::SurfaceFill, Emphasis::Plain);
            let last = app.screen.columns() - MARGIN - 1;
            let pad = "\u{258e}";

            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let summary = at(&rows, "✓ edit 2").expect("the summary row");
            let y = u16::try_from(summary).expect("a row");
            assert_eq!(rows[summary - 1], pad, "the pad above:\n{shown}");
            assert_eq!(style(&app, last, y - 1), surface, "the pad above:\n{shown}");
            // The folded run's newest diff draws inside the summary's card, so
            // the pad under the summary row comes after it.
            let below = (summary + 1..rows.len())
                .find(|&at| rows[at] == pad)
                .expect("the pad below the summary row");
            assert_eq!(
                style(&app, last, u16::try_from(below).expect("a row")),
                surface,
                "the pad below:\n{shown}"
            );

            app.screen.clear();
            app.folds.toggle(fold);
            paint_summary(&mut app, &run);
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let summary = at(&rows, "✓ edit 2").expect("the summary row");
            let first = at(&rows, "Edit src/a.rs").expect("the empty edit's title");
            let second = at(&rows, "Edit src/b.rs").expect("the second edit's title");
            let bash = at(&rows, "$ cargo test").expect("the bash card's title");
            // The summary's pad, a blank row, then the first card's pad.
            assert_eq!(first, summary + 4, "{shown}");
            // The empty edit: its pad, its title, its pad, a blank row, the
            // next card's pad.
            assert_eq!(second, first + 4, "{shown}");
            for (gap, what) in [
                (summary + 2, "under the summary"),
                (first + 2, "between the edits"),
                (bash - 2, "above the bash card"),
            ] {
                let y = u16::try_from(gap).expect("a row");
                assert_eq!(rows[gap], "\u{2502}", "the gap {what}:\n{shown}");
                assert_ne!(style(&app, last, y), surface, "the gap {what}:\n{shown}");
            }
            for padding in [summary + 1, first - 1, first + 1, bash - 1] {
                let y = u16::try_from(padding).expect("a row");
                assert_eq!(style(&app, last, y), surface, "pad {padding}:\n{shown}");
            }
        }

        /// A run's summary row is a one-row card on the surface, red when a call
        /// failed, and the cards of an open run draw on the surface too: the
        /// run's own paints have no fill, which its cards must not take.
        #[test]
        fn summary_row_and_cards_of_a_run_draw_on_the_surface() {
            let mut ok = app(true);
            let fold = ok.folds.add();
            let run = Group::new(
                vec![
                    call("1", "read", r#"{"path":"src/a.rs"}"#),
                    call("2", "bash", r#"{"command":"cargo test"}"#),
                ],
                fold,
            );
            paint_summary(&mut ok, &run);
            let surface = ok.style(Role::SurfaceFill, Emphasis::Plain);
            let last = ok.screen.columns() - MARGIN - 1;
            let line = row(&ok, 2);
            assert!(line.starts_with("\u{258e} ✓ read 1 file"), "{line}");
            assert_eq!(style(&ok, last, 2), surface, "the summary row: {line}");

            ok.screen.clear();
            ok.folds.toggle(fold);
            paint_summary(&mut ok, &run);
            let rows = screen_rows(&ok);
            let title = at(&rows, "$ cargo test").expect("the bash card's title");
            let y = u16::try_from(title).expect("a row");
            assert_eq!(
                style(&ok, last, y),
                surface,
                "a card inside an open run:\n{}",
                rows.join("\n")
            );

            let mut red = app(true);
            let fold = red.folds.add();
            let run = Group::new(
                vec![
                    call("1", "bash", r#"{"command":"cargo test"}"#),
                    failed("2", "bash", r#"{"command":"cargo clippy"}"#),
                ],
                fold,
            );
            paint_summary(&mut red, &run);
            let line = row(&red, 2);
            assert_eq!(
                style(&red, last, 2),
                red.style(Role::ErrFill, Emphasis::Plain),
                "a run with a failure: {line}"
            );
        }

        /// A run whose calls all succeeded draws its summary row and the lines of
        /// its newest edit alone: no card, and no rows kept for one.
        #[test]
        fn folded_failed_card_absent_when_no_call_failed() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    call("1", "bash", r#"{"command":"cargo test"}"#),
                    edited("src/app/mod.rs", 5, 1),
                    call("2", "bash", r#"{"command":"cargo fmt"}"#),
                ],
                fold,
            );
            let folds = app.folds.clone();
            let entries = [entry(fold, Block::Group(run.row()))];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &folds);
            assert_eq!(
                drawn, 9,
                "a blank row, the summary's padding row, its row, the newest edit's\
                 three lines and its count, the padding row under them and a blank row"
            );
            let rows = screen_rows(&app);
            assert!(
                !rows.iter().any(|line| line.contains('\u{2717}')),
                "no failed card draws without a failure:\n{}",
                rows.join("\n")
            );
            assert!(
                at(&rows, "1 failed").is_none(),
                "the chip counts no failure:\n{}",
                rows.join("\n")
            );
        }

        /// A resumed session folds a run of its history the way a live turn does,
        /// and keeps the card of the call that failed under the summary row.
        #[test]
        fn folded_failed_card_in_history_after_a_resume() {
            let calls = vec![
                ToolCall {
                    id: Text::from("call-1"),
                    name: Text::from("bash"),
                    arguments: Text::from(r#"{"command":"cargo nextest run -p reagent-tui"}"#),
                },
                ToolCall {
                    id: Text::from("call-2"),
                    name: Text::from("edit"),
                    arguments: Text::from(
                        r#"{"path":"src/app/mod.rs","old_string":"old\n","new_string":"new\n"}"#,
                    ),
                },
                ToolCall {
                    id: Text::from("call-3"),
                    name: Text::from("bash"),
                    arguments: Text::from(r#"{"command":"cargo fmt"}"#),
                },
            ];
            let answer = |id: &str, content: &str, is_error: bool| {
                Message::ToolResult(ToolResult {
                    call_id: Text::from(id),
                    content: Text::from(content),
                    is_error,
                    images: Vec::new(),
                })
            };
            let mut messages = VecDeque::from([Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("fixing the page test"),
                }],
            ))]);
            messages.push_back(answer("call-1", "no such file: the page test", true));
            messages.push_back(answer("call-2", "written", false));
            messages.push_back(answer("call-3", "formatted", false));
            messages.push_back(Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("done"),
                }],
            )));
            let mut app = app(true);
            app.history(&mut Tail {
                messages,
                checkpoints: Vec::new(),
                calls,
                omitted: 0,
                pictures: Vec::new(),
            });
            let group = match app.pending.remove(1) {
                Item::Group(group) => group,
                other => panic!("the history's run of three calls is one row, got {other:?}"),
            };
            assert_eq!(group.failed, 1, "the first call failed");
            paint_summary(&mut app, &group);
            let rows = screen_rows(&app);
            let top = at(&rows, "✗ bash 2 · edit 1")
                .unwrap_or_else(|| panic!("the summary row draws:\n{}", rows.join("\n")));
            assert_eq!(
                at(&rows, "▎ ✗ $ cargo nextest run -p reagent-tui"),
                Some(top + 5),
                "the card draws under the summary row:\n{}",
                rows.join("\n")
            );
            assert_eq!(
                rows[top + 7],
                "▎ no such file: the page test",
                "the card keeps the error the session recorded"
            );
        }

        /// The summary row sums the lines its edits add and remove, and names the
        /// language they all share.
        #[test]
        fn summary_row_sums_the_edits_and_names_their_one_language() {
            let mut owned = app(true);
            let fold = owned.folds.add();
            let rust = Group::new(
                vec![edited("src/a.rs", 5, 1), edited("src/b.rs", 2, 1)],
                fold,
            );
            paint_summary(&mut owned, &rust);
            let line = row(&owned, 2);
            assert!(line.ends_with("rust +7 -2  ▸"), "{line}");
            let surface = Role::SurfaceFill;
            assert_eq!(
                style(&owned, column(&line, "rust"), 2),
                over(&mut owned, Role::Dim, Emphasis::Plain, surface),
                "the language is dim: {line}"
            );
            assert_eq!(
                style(&owned, column(&line, "+7"), 2),
                over(&mut owned, Role::Add, Emphasis::Plain, surface),
                "the added lines take the add role: {line}"
            );
            assert_eq!(
                style(&owned, column(&line, "-2"), 2),
                over(&mut owned, Role::Delete, Emphasis::Plain, surface),
                "the removed lines take the delete role: {line}"
            );

            let mut mixed = app(false);
            let fold = mixed.folds.add();
            let two = Group::new(
                vec![edited("src/a.rs", 5, 1), edited("src/b.py", 2, 1)],
                fold,
            );
            paint_summary(&mut mixed, &two);
            let line = row(&mixed, 2);
            assert!(line.ends_with("+7 -2  ▸"), "{line}");
            assert!(
                !line.contains("rust") && !line.contains("python"),
                "two languages leave the name out: {line}"
            );
        }

        /// The summary row keeps the wording of the reads: a count of files.
        #[test]
        fn summary_row_keeps_read_wording() {
            let mut owned = app(true);
            paint_summary(&mut owned, &group(1));
            let line = row(&owned, 2);
            assert!(
                line.starts_with("\u{258e} ✓ read 3 files · grep 2"),
                "{line}"
            );

            let mut single = app(false);
            let fold = single.folds.add();
            let run = Group::new(
                vec![
                    answered("1", "read", r#"{"path":"src/app/mod.rs"}"#, 20),
                    answered("2", "grep", r#"{"pattern":"fn paint"}"#, 3),
                ],
                fold,
            );
            paint_summary(&mut single, &run);
            let line = row(&single, 2);
            assert!(
                line.starts_with("\u{258e} ✓ read 1 file · grep 1"),
                "{line}"
            );
        }

        /// A run of one call keeps its own row.
        #[test]
        fn summary_lone_call_keeps_its_own_row() {
            let mut app = app(true);
            app.live.push(Item::Tool(Box::new(call(
                "1",
                "bash",
                r#"{"command":"cargo test"}"#,
            ))));
            app.freeze_all();
            assert!(
                matches!(app.pending.as_slice(), [Item::Tool(_)]),
                "one call is one row: {:?}",
                app.pending
            );
        }

        /// A run of two calls of any tool leaves the live area as one row.
        #[test]
        fn summary_run_of_two_calls_is_one_group() {
            let mut app = app(true);
            for (id, name, arguments) in [
                ("1", "bash", r#"{"command":"cargo test"}"#),
                ("2", "read", r#"{"path":"src/app/mod.rs"}"#),
            ] {
                app.live
                    .push(Item::Tool(Box::new(call(id, name, arguments))));
            }
            app.freeze_all();
            match app.pending.as_slice() {
                [Item::Group(group)] => {
                    assert_eq!(group.calls, 2, "both calls are in the run");
                    assert_eq!(group.names.len(), 2, "one name each");
                }
                other => panic!("a run of two calls is one row, got {other:?}"),
            }
        }

        /// Tab on a folded run opens it into one row per call.
        #[test]
        fn summary_opens_into_one_row_per_call_on_tab() {
            let mut app = app(true);
            let mut tools = vec![
                call("1", "bash", r#"{"command":"cargo test"}"#),
                edited("src/a.rs", 1, 0),
                call("2", "bash", r#"{"command":"cargo clippy"}"#),
                edited("src/b.rs", 1, 0),
            ];
            // The run numbers its calls as it takes them, then the run itself.
            for tool in &mut tools {
                tool.fold = app.folds.add();
            }
            let fold = app.folds.add();
            let newest = tools[3].fold;
            app.folds.show_diff(newest);
            app.live
                .push(Item::Group(Box::new(Group::new(tools, fold))));
            app.screen.clear();
            let drawn = app.paint_live(0, ROWS, 0);
            assert_eq!(
                drawn, 6,
                "a blank row, the summary's padding row, its row, the line of its\
                 newest diff, its padding row and a blank row"
            );
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.screen.clear();
            let drawn = app.paint_live(0, ROWS, 0);
            assert_eq!(
                drawn, 27,
                "a blank row, the summary's padding row, its row, the padding row under \
                 it, a card per call between blank rows, the newest edit's one diff \
                 line and a blank row"
            );
            let rows = screen_rows(&app);
            assert!(
                rows[2].starts_with("\u{2502} ✓ bash 2 · edit 2"),
                "the summary row:\n{}",
                rows.join("\n")
            );
            let calls = [
                "$ cargo test",
                "← Edit src/a.rs",
                "$ cargo clippy",
                "← Edit src/b.rs",
            ];
            let mut last = 2;
            for (index, want) in calls.into_iter().enumerate() {
                let line = at(&rows, want).unwrap_or_else(|| {
                    panic!(
                        "call {index} draws under the summary row, wanted {want:?}:\n{}",
                        rows.join("\n")
                    )
                });
                assert!(
                    line > last,
                    "call {index} draws in its order:\n{}",
                    rows.join("\n")
                );
                last = line;
            }
        }

        /// A run of `tools` in the app's fold state: each call takes a number as
        /// joining a run numbers it, then the run itself, and the newest call is
        /// the one whose diff the run shows.
        fn numbered(app: &mut App, mut tools: Vec<Tool>) -> (Group, u64) {
            for tool in &mut tools {
                tool.fold = app.folds.add();
            }
            if let Some(newest) = tools.last().map(|tool| tool.fold) {
                app.folds.show_diff(newest);
            }
            let fold = app.folds.add();
            (Group::new(tools, fold), fold)
        }

        /// The rows of the screen as one string with the spaces gone: a row's
        /// wording is read without the spaces between its parts.
        fn flat(rows: &[String]) -> String {
            rows.join("\n")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        }

        /// The rows the ring stores, their text from the margin.
        fn ring_lines(app: &App) -> Vec<String> {
            let len = app.screen.transcript().ring_len();
            (0..len)
                .map(|index| ring_line(app, u64::try_from(index).unwrap_or(u64::MAX)))
                .collect()
        }

        /// A click on a call's row inside an open run opens that call, and its
        /// diff draws whole under its row.
        #[test]
        fn member_click_on_an_edit_opens_its_diff() {
            let mut app = app(true);
            let (run, _) = numbered(
                &mut app,
                vec![
                    call("1", "bash", r#"{"command":"cargo test"}"#),
                    edited("src/a.rs", 9, 0),
                ],
            );
            app.live.push(Item::Group(Box::new(run)));
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.paint();
            let rows = screen_rows(&app);
            let edit = at(&rows, "← Edit src/a.rs").expect("the edit's row");
            assert!(
                flat(&rows).contains("morelines"),
                "the run holds the rest of the edit back:\n{}",
                rows.join("\n")
            );

            app.clicked(crate::mouse::Cell {
                column: 3,
                row: u16::try_from(edit).expect("a row"),
            });
            app.paint();
            let rows = screen_rows(&app);
            assert!(
                !flat(&rows).contains("morelines"),
                "the click opens the whole diff of the edit:\n{}",
                rows.join("\n")
            );
            assert!(
                flat(&rows).matches("+new").count() >= 9,
                "every line of the edit draws under its row:\n{}",
                rows.join("\n")
            );
        }

        /// Tab on an open run opens its newest call, whose error draws under its
        /// row when the call failed.
        #[test]
        fn member_tab_on_a_failed_call_shows_its_error() {
            let mut app = app(true);
            let (run, _) = numbered(
                &mut app,
                vec![
                    call("1", "bash", r#"{"command":"cargo test"}"#),
                    failed("2", "bash", r#"{"command":"cargo run"}"#),
                ],
            );
            app.pending.push(Item::Group(Box::new(run)));
            app.commit();
            app.paint();
            // The first tab opens the run, the second its newest call.
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            let lines = ring_lines(&app);
            assert!(
                lines.iter().any(|line| line.trim() == "▎ no such file"),
                "the failed call's error draws under its title:\n{}",
                lines.join("\n")
            );
        }

        /// A second click on a call's row inside an open run folds it back, and
        /// the rows of its diff leave the run.
        #[test]
        fn member_click_again_folds_it() {
            let mut app = app(true);
            let (run, _) = numbered(
                &mut app,
                vec![
                    call("1", "bash", r#"{"command":"cargo test"}"#),
                    edited("src/a.rs", 9, 0),
                ],
            );
            app.live.push(Item::Group(Box::new(run)));
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.paint();
            let rows = screen_rows(&app);
            let edit = at(&rows, "← Edit src/a.rs").expect("the edit's row");
            let cell = crate::mouse::Cell {
                column: 3,
                row: u16::try_from(edit).expect("a row"),
            };
            app.clicked(cell);
            app.paint();
            assert!(
                !flat(&screen_rows(&app)).contains("morelines"),
                "the click opens the edit"
            );

            app.clicked(cell);
            app.paint();
            let rows = screen_rows(&app);
            assert!(
                flat(&rows).contains("morelines"),
                "the second click folds the edit back:\n{}",
                rows.join("\n")
            );
            assert!(
                flat(&rows).matches("+new").count() < 9,
                "its lines leave the run again:\n{}",
                rows.join("\n")
            );
        }

        /// Ctrl+O opens every block and every call inside it.
        #[test]
        fn member_ctrl_o_opens_every_member() {
            let mut app = app(true);
            let (run, _) = numbered(
                &mut app,
                vec![edited("src/a.rs", 9, 0), edited("src/b.rs", 9, 0)],
            );
            app.pending.push(Item::Group(Box::new(run)));
            app.commit();
            app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
            let lines = ring_lines(&app);
            assert_eq!(
                lines.len(),
                33,
                "a blank row, the summary's padding row, its row, the padding row under \
                 it, two cards of thirteen rows each between blank rows and a blank \
                 row:\n{}",
                lines.join("\n")
            );
            assert!(
                !lines.join("\n").contains("more lines"),
                "no call keeps a count row open:\n{}",
                lines.join("\n")
            );
        }

        /// The newest edit of an open run keeps the four lines of its diff even
        /// when that call is not opened.
        #[test]
        fn member_newest_edit_keeps_its_four_lines_in_an_open_group() {
            let mut app = app(true);
            let (run, _) = numbered(
                &mut app,
                vec![edited("src/a.rs", 9, 0), edited("src/b.rs", 9, 0)],
            );
            app.pending.push(Item::Group(Box::new(run)));
            app.commit();
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            let lines = ring_lines(&app);
            assert_eq!(
                lines.len(),
                19,
                "a blank row, the summary's padding row, its row, the padding row under \
                 it, two cards between blank rows, the four lines of the newest diff, \
                 its count and a blank row:\n{}",
                lines.join("\n")
            );
            assert!(
                lines[16].ends_with("5 more lines · tab opens"),
                "the newest edit says the tab opens the rest: {:?}",
                lines[16]
            );
            assert!(
                !lines[4..8].iter().any(|line| line.contains("more lines")),
                "the older edit keeps no preview of its own: {:?}",
                &lines[4..8]
            );
        }

        /// A resumed session folds the runs of its history the way a live turn
        /// does: the text of a reply ends the run it follows.
        #[test]
        fn summary_history_groups_a_run_of_calls() {
            let calls: Vec<ToolCall> = (0..3)
                .map(|index| ToolCall {
                    id: Text::from(format!("call-{index}")),
                    name: Text::from("bash"),
                    arguments: Text::from(format!(r#"{{"command":"step {index}"}}"#)),
                })
                .collect();
            let mut messages = VecDeque::from([Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("running the three steps"),
                }],
            ))]);
            for call in &calls {
                messages.push_back(Message::ToolResult(ToolResult {
                    call_id: call.id.clone(),
                    content: Text::from("ok"),
                    is_error: false,
                    images: Vec::new(),
                }));
            }
            messages.push_back(Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("done"),
                }],
            )));
            let mut app = app(true);
            app.history(&mut Tail {
                messages,
                checkpoints: Vec::new(),
                calls,
                omitted: 0,
                pictures: Vec::new(),
            });
            assert!(
                matches!(app.pending.first(), Some(Item::Reply { .. })),
                "the reply comes before the run: {:?}",
                app.pending
            );
            let group = match app.pending.remove(1) {
                Item::Group(group) => group,
                other => panic!("the history's run of three calls is one row, got {other:?}"),
            };
            assert!(
                matches!(app.pending.last(), Some(Item::Reply { .. })),
                "the reply after the run stays after it: {:?}",
                app.pending
            );
            assert_eq!(group.calls, 3, "the three calls are one run");
            assert_eq!(group.names.len(), 1, "one name for the run");
            assert_eq!(group.names[0].count, 3, "with its count");
            paint_summary(&mut app, &group);
            let line = row(&app, 2);
            assert!(line.starts_with("\u{258e} ✓ bash 3"), "{line}");
        }

        /// Drives one turn whose only call is `name`, called with `arguments`
        /// and answered with `out`, and commits what the turn leaves.
        fn turn(app: &mut App, id: &str, name: &str, arguments: &str, out: &str) {
            app.frame(Frame::TurnStart);
            app.frame(Frame::ToolStart {
                id: id.into(),
                name: name.into(),
            });
            app.frame(Frame::ToolCallReady {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            });
            app.frame(Frame::ToolResult {
                images: 0,
                id: id.into(),
                failed: false,
                bytes: out.len(),
                error: None,
                output: None,
                earlier: 0,
            });
            app.fill(id, out);
            app.frame(Frame::TurnEnd);
            app.commit();
        }

        /// The rows the transcript of the whole screen draws, the header row of
        /// the screen left out.
        fn painted(app: &mut App) -> Vec<String> {
            app.paint();
            // The row the screen keeps above the transcript, and the blank rows
            // a run of tool rows keeps above it, are not the test's.
            let mut rows = screen_rows(app).split_off(1);
            let air = rows.iter().take_while(|row| row.is_empty()).count();
            rows.split_off(air)
        }

        /// Four turns with one call each and a reply at the end: the four calls
        /// are one run, drawn as one row.
        #[test]
        fn summary_calls_in_separate_turns_form_one_run() {
            let mut app = app(true);
            for (index, name) in ["bash", "edit", "bash", "edit"].into_iter().enumerate() {
                let id = format!("{}", index + 1);
                let arguments = match name {
                    "bash" => format!(r#"{{"command":"step {index}"}}"#),
                    _ => format!(
                        r#"{{"path":"src/{index}.rs","old_string":"old","new_string":"new"}}"#
                    ),
                };
                turn(&mut app, &id, name, &arguments, "ok");
            }
            app.frame(Frame::TextDelta("done".into()));
            app.frame(Frame::TurnEnd);
            app.commit();
            let rows = painted(&mut app);
            assert!(
                rows[1].starts_with("\u{258e} ✓ bash 2 · edit 2"),
                "one row for the four calls:\n{}",
                rows.join("\n")
            );
            assert_eq!(
                app.screen.transcript().blocks(),
                1,
                "the four calls grew one block of the ring"
            );
            assert_eq!(
                rows.iter().filter(|row| row.contains("bash  step")).count(),
                0,
                "no call draws a row of its own:\n{}",
                rows.join("\n")
            );
        }

        /// A run that still goes draws one row per call, and one summary row once
        /// the run ends.
        #[test]
        fn summary_run_stays_open_while_it_runs() {
            let mut app = app(true);
            let commands = ["cargo test", "cargo clippy", "cargo fmt"];
            for (index, command) in commands.into_iter().enumerate() {
                let id = format!("{}", index + 1);
                let arguments = format!(r#"{{"command":"{command}"}}"#);
                turn(&mut app, &id, "bash", &arguments, "ok");
            }
            let rows = painted(&mut app);
            let mut last = 0;
            for (index, command) in commands.into_iter().enumerate() {
                let line = at(&rows, command).unwrap_or_else(|| {
                    panic!("call {index} draws its own row:\n{}", rows.join("\n"))
                });
                assert!(
                    line > last,
                    "call {index} draws in its order:\n{}",
                    rows.join("\n")
                );
                last = line;
            }
            assert!(
                !rows[0].starts_with("✓ bash 3"),
                "the summary row comes at the run's end:\n{}",
                rows.join("\n")
            );
            app.frame(Frame::RunEnd {
                end: RunEnd::EndTurn,
            });
            app.commit();
            let rows = painted(&mut app);
            assert!(
                rows[1].starts_with("\u{258e} ✓ bash 3"),
                "the run ends in one row:\n{}",
                rows.join("\n")
            );
            assert_eq!(
                app.screen.transcript().blocks(),
                1,
                "the three calls grew one block of the ring"
            );
            assert!(
                !rows[2].contains("cargo"),
                "no call draws a row of its own any more:\n{}",
                rows.join("\n")
            );
        }

        /// The thoughts of a run belong to it: they draw in their order between
        /// its calls while it goes, and its row counts the calls alone.
        #[test]
        fn summary_thoughts_join_the_run_and_show_when_open() {
            let mut app = app(true);
            turn(&mut app, "1", "bash", r#"{"command":"cargo test"}"#, "ok");
            app.frame(Frame::ThinkingDelta("now the lints".into()));
            turn(&mut app, "2", "bash", r#"{"command":"cargo clippy"}"#, "ok");
            let rows = painted(&mut app);
            let first = at(&rows, "cargo test").expect("the first call draws");
            let thought = at(&rows, "thought").expect("the thought draws between the calls");
            let second = at(&rows, "cargo clippy").expect("the second call draws");
            assert!(
                first < thought && thought < second,
                "the thought draws in its order:\n{}",
                rows.join("\n")
            );
            app.frame(Frame::RunEnd {
                end: RunEnd::EndTurn,
            });
            app.commit();
            let rows = painted(&mut app);
            assert!(
                rows[1].starts_with("\u{258e} ✓ bash 2"),
                "the row counts the calls and no thought:\n{}",
                rows.join("\n")
            );
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.paint();
            let rows = screen_rows(&app);
            assert!(
                at(&rows, "now the lints").is_some(),
                "the thought shows where the run was opened:\n{}",
                rows.join("\n")
            );
        }

        /// A user message ends the run before it: the calls after it form a run of
        /// their own.
        #[test]
        fn summary_user_message_ends_the_run() {
            let mut app = app(true);
            turn(&mut app, "1", "bash", r#"{"command":"cargo test"}"#, "ok");
            app.frame(Frame::UserMessage {
                text: "and the lints".into(),
                images: 0,
            });
            app.commit();
            turn(&mut app, "2", "bash", r#"{"command":"cargo clippy"}"#, "ok");
            app.frame(Frame::RunEnd {
                end: RunEnd::EndTurn,
            });
            app.commit();
            let rows = painted(&mut app);
            let user = at(&rows, "and the lints").expect("the message draws");
            let first = at(&rows, "cargo test").expect("the call before it draws");
            let second = at(&rows, "cargo clippy").expect("the call after it draws");
            assert!(
                first < user && user < second,
                "the message splits the two runs:\n{}",
                rows.join("\n")
            );
            assert!(
                at(&rows, "bash 2").is_none(),
                "no run holds both calls:\n{}",
                rows.join("\n")
            );
        }

        /// A run of one call draws that call's own row once the run ends.
        #[test]
        fn summary_one_call_run_keeps_its_row() {
            let mut app = app(true);
            turn(&mut app, "1", "bash", r#"{"command":"cargo test"}"#, "ok");
            app.frame(Frame::RunEnd {
                end: RunEnd::EndTurn,
            });
            app.commit();
            let rows = painted(&mut app);
            assert!(
                at(&rows, "$ cargo test").is_some(),
                "the call keeps its card:\n{}",
                rows.join("\n")
            );
            assert!(
                at(&rows, "bash 1").is_none(),
                "a run of one call draws no summary row:\n{}",
                rows.join("\n")
            );
        }

        /// A resumed session folds the calls of its turns the way a live run does:
        /// a turn that asked for a call alone does not end the run.
        #[test]
        fn summary_history_run_spans_turns() {
            let calls: Vec<ToolCall> = (0..2)
                .map(|index| ToolCall {
                    id: Text::from(format!("call-{index}")),
                    name: Text::from("bash"),
                    arguments: Text::from(format!(r#"{{"command":"step {index}"}}"#)),
                })
                .collect();
            let mut messages = VecDeque::new();
            for call in &calls {
                messages.push_back(Message::assistant(AssistantMessage::new(
                    "fake",
                    "fake-model",
                    vec![Content::ToolCall(call.clone())],
                )));
                messages.push_back(Message::ToolResult(ToolResult {
                    call_id: call.id.clone(),
                    content: Text::from("ok"),
                    is_error: false,
                    images: Vec::new(),
                }));
            }
            messages.push_back(Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("done"),
                }],
            )));
            let mut app = app(true);
            app.history(&mut Tail {
                messages,
                checkpoints: Vec::new(),
                calls,
                omitted: 0,
                pictures: Vec::new(),
            });
            assert!(
                matches!(app.pending.first(), Some(Item::Group(group)) if group.calls == 2),
                "the two turns are one run: {:?}",
                app.pending
            );
            let group = match app.pending.remove(0) {
                Item::Group(group) => group,
                other => panic!("the two turns are one run, got {other:?}"),
            };
            paint_summary(&mut app, &group);
            let line = row(&app, 2);
            assert!(line.starts_with("\u{258e} ✓ bash 2"), "{line}");
        }

        /// A live group of three reads and two greps, each of its calls printing
        /// one line: Ctrl+O opens it into the group's row and, under it, one row
        /// per call with the line it printed.
        #[test]
        fn ctrl_o_opens_a_live_group_into_one_row_per_call() {
            let mut app = app(true);
            let fold = app.folds.add();
            let calls = [
                ("1", "read", r#"{"path":"src/app/mod.rs"}"#),
                ("2", "read", r#"{"path":"src/app/fold.rs"}"#),
                ("3", "read", r#"{"path":"src/app/full.rs"}"#),
                ("4", "grep", r#"{"pattern":"fn paint"}"#),
                ("5", "grep", r#"{"pattern":"members"}"#),
            ];
            let tools = calls
                .into_iter()
                .map(|(id, name, arguments)| answered(id, name, arguments, 1))
                .collect();
            app.live
                .push(Item::Group(Box::new(Group::new(tools, fold))));
            app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
            app.screen.clear();
            let drawn = app.paint_live(0, ROWS, 0);
            assert_eq!(
                drawn, 20,
                "a blank row, the group's padding row, its row, the padding row \
                 and a blank row under it, a row, its line and a blank row per call"
            );
            assert_eq!(app.live_rows(), 20, "the count is the rows it draws");
            let rows = screen_rows(&app);
            assert!(
                rows[2].starts_with("\u{2502} ✓ read 3 files \u{b7} grep 2"),
                "the group's own row:\n{}",
                rows.join("\n")
            );
            let wants = [
                "→ Read src/app/mod.rs",
                "→ Read src/app/fold.rs",
                "→ Read src/app/full.rs",
                "✱ Grep \"fn paint\"",
                "✱ Grep \"members\"",
            ];
            let last = wants.len() - 1;
            for (index, want) in wants.into_iter().enumerate() {
                let line = &rows[5 + 3 * index];
                assert!(
                    line.contains(want),
                    "call {index} draws under the group's row, wanted {want:?}:\n{}",
                    rows.join("\n")
                );
                // The run's line ends at its last member: the rows under that
                // member's branch keep the columns before the text blank.
                let lead = if index == last {
                    "      x"
                } else {
                    "\u{2502}     x"
                };
                assert_eq!(
                    rows[6 + 3 * index],
                    lead,
                    "the line call {index} printed follows its row:\n{}",
                    rows.join("\n")
                );
            }
            assert!(
                rows[19].is_empty(),
                "no row past the last call:\n{}",
                rows.join("\n")
            );
            // A folded group keeps its row between two padding rows, between two
            // blank rows: the calls draw only open.
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.screen.clear();
            assert_eq!(app.paint_live(0, ROWS, 0), 5);
        }

        /// Tab on a live thought opens it, and the text box starts two rows under
        /// the block's last drawn row, its blank row and the box's empty row
        /// between them: the live area counts
        /// the open block's lines, so the box and the status line never collide
        /// with them.
        #[test]
        fn tab_on_a_live_thought_puts_the_box_under_its_last_row() {
            let mut app = app(true);
            let fold = app.folds.add();
            let think = Thinking {
                text: "the newest line of the reasoning".to_owned(),
                since: Instant::now(),
                seconds: 4,
                fold,
            };
            app.live.push(Item::Thought(Box::new(think)));
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.paint();
            let rows = screen_rows(&app);
            let thought = at(&rows, "thought for 4s").expect("the block's row draws");
            let last = at(&rows, "the newest line of the reasoning").expect("the open line draws");
            assert_eq!(last, thought + 1, "the line sits under the block's row");
            assert!(rows[last + 1].is_empty(), "one blank row after the block");
            assert!(rows[last + 2].is_empty(), "the empty row above the box");
            assert!(
                line(&app, (last + 3) as u16).starts_with('\u{258e}'),
                "the box starts under the empty row above it:\n{}",
                rows.join("\n")
            );
            assert!(
                rows[last + 4].contains("Ask anything"),
                "the box's text row follows:\n{}",
                rows.join("\n")
            );
            let status = at(&rows, "idle").expect("the status line draws");
            assert!(
                rows[status - 1].is_empty(),
                "the empty row between the box and the status line:\n{}",
                rows.join("\n")
            );
        }

        #[test]
        fn fold_and_open_a_diff_under_an_edit() {
            let mut app = at_width(100);
            let mut folds = Folds::new();
            let id = folds.add();
            let edit = Edit::new();
            let entries = [entry(id, Block::Tool(edit.row()))];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &folds);
            assert_eq!(
                drawn,
                1 + 3 + DIFF_FOLD_LINES as u16 + 1 + 1 + 1,
                "a blank row, the card's three rows, four diff lines, the count, the \
                 card's last row and a blank row"
            );
            assert!(
                row(&app, 2).ends_with("+14 -3  ▸"),
                "a folded block is marked folded"
            );
            let second = row(&app, 4);
            assert!(
                second.starts_with("▎") && second.contains("636"),
                "the line keeps its number in the card: {second:?}"
            );
            let removed = row(&app, 5);
            assert!(removed.contains("- let mut tui = Tui::start"), "{removed}");
            assert_eq!(
                style(&app, 8, 5),
                app.diff_line(Some(Role::SurfaceFill), DiffKind::Delete)
                    .sign,
                "a removed line is the delete role over its tint"
            );
            assert_eq!(row(&app, 8), "▎ 5 more lines · tab opens");
            assert_eq!(
                style(&app, 2, 8),
                over(&mut app, Role::Dim, Emphasis::Plain, Role::SurfaceFill),
                "the count is dim"
            );
            folds.toggle(id);
            app.screen.clear();
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &folds);
            assert_eq!(
                drawn,
                1 + 3 + EDIT.len() as u16 + 1 + 1,
                "the tab opens every line between two blank rows"
            );
            assert!(
                row(&app, 2).ends_with("+14 -3  ▾"),
                "an open block is marked open"
            );
            assert_eq!(
                style(&app, 8, 6),
                app.diff_line(Some(Role::SurfaceFill), DiffKind::Add).sign,
                "an added line is the add role over its tint"
            );
        }

        /// A finished edit of one file, as the app holds it after its result.
        fn finished(arguments: &str) -> Tool {
            let mut tool = Tool::new("call-1", "edit");
            tool.set_call("edit", arguments, COLUMNS);
            tool.set_result(2, None, COLUMNS);
            tool
        }

        /// Paints the card of `tool` from the top.
        fn paint_one(app: &mut App, tool: &Tool) {
            let mut spans = [Span {
                text: "",
                role: SpanRole::Dim,
            }; SPANS];
            let mut folds = Folds::new();
            let id = folds.add();
            let row = tool.row(&mut spans);
            app.paint_blocks(0, 0, ROWS, &[entry(id, Block::Tool(row))], &folds);
        }

        /// The rows the diff of [`EDIT`] takes when the edit is open, the rows of
        /// its card left out.
        fn paint_edit(app: &mut App) -> Vec<String> {
            let mut folds = Folds::new();
            let id = folds.add();
            folds.toggle(id);
            let edit = Edit::new();
            let entries = [entry(id, Block::Tool(edit.row()))];
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
            let mut rows = screen_rows(app);
            rows.truncate(CARD_ROWS + EDIT.len());
            rows.split_off(CARD_ROWS)
        }

        /// The language of the file an edit changes sits before its counts, dim.
        #[test]
        fn diff_row_names_the_language_before_the_counts() {
            let tool =
                finished(r#"{"path":"src/app.rs","old_string":"one\ntwo","new_string":"three"}"#);
            let mut app = app(true);
            paint_one(&mut app, &tool);
            let line = row(&app, 2);
            assert!(line.ends_with("rust  +1 -2  \u{25b8}"), "{line}");
            let x = column(&line, "rust");
            assert_eq!(
                style(&app, x, 2),
                over(&mut app, Role::Dim, Emphasis::Plain, Role::SurfaceFill),
                "the language is dim: {line}"
            );
        }

        /// The code of a diff line draws in the roles of its language's tokens,
        /// over the tint of its line.
        #[test]
        fn diff_lines_are_highlighted() {
            let tool = finished(
                "{\"path\":\"src/a.rs\",\"old_string\":\"let a = 1;\",\"new_string\":\"let b = \\\"two\\\";\"}",
            );
            let mut app = at_width(100);
            paint_one(&mut app, &tool);
            let added = app.diff_line(Some(Role::SurfaceFill), DiffKind::Add);
            assert_eq!(
                style(&app, 10, 5),
                added.token(Token::Keyword),
                "`let` is a keyword of the added line"
            );
            assert_eq!(
                style(&app, 18, 5),
                added.token(Token::String),
                "`\"two\"` is a string of the added line"
            );
            assert_ne!(
                added.token(Token::Keyword),
                added.token(Token::Text),
                "the keyword does not draw as plain text"
            );
            let removed = app.diff_line(Some(Role::SurfaceFill), DiffKind::Delete);
            assert_ne!(
                added.token(Token::Text),
                removed.token(Token::Text),
                "an added line and a removed one sit on their own tint"
            );
        }

        /// At 120 columns a diff draws its old lines at the left and its new lines
        /// at the right, a context line on the same row of both.
        #[test]
        fn diff_side_by_side_at_120_columns() {
            let mut app = app(true);
            let rows = paint_edit(&mut app);
            let context = &rows[0];
            assert_eq!(
                context.matches("let provider = home.script").count(),
                2,
                "the context line draws on both halves: {context:?}"
            );
            assert_eq!(
                context.chars().nth(57),
                Some('\u{2502}'),
                "the rule between the halves: {context:?}"
            );
            let changed = &rows[1];
            let removed = changed
                .find("- let mut tui = Tui::start")
                .expect("the removed line is drawn");
            let added = changed
                .find("+ let mut start = Duration::MAX;")
                .expect("the added line is drawn");
            assert!(removed < 57, "the old line is at the left: {changed:?}");
            assert!(added > 57, "the new line is at the right: {changed:?}");
        }

        /// Under 120 columns the diff keeps one column, its lines highlighted.
        #[test]
        fn diff_one_column_under_120() {
            let mut app = at_width(119);
            let rows = paint_edit(&mut app);
            assert!(
                !rows.iter().any(|row| row.contains('\u{2502}')),
                "no rule under 120 columns: {rows:?}"
            );
            assert_eq!(
                rows[0].matches("let provider = home.script").count(),
                1,
                "one column draws the context line once: {:?}",
                rows[0]
            );
            assert!(
                rows[1].contains("- let mut tui = Tui::start"),
                "the removed line is its own row: {:?}",
                rows[1]
            );
            assert!(
                rows[2].contains("+ let mut start = Duration::MAX;"),
                "the added line is the row under it: {:?}",
                rows[2]
            );
        }

        /// A line without a partner leaves the half it would sit in on the gap
        /// fill.
        #[test]
        fn diff_gap_fills_an_unpaired_line() {
            let mut app = app(true);
            let rows = paint_edit(&mut app);
            let added = &rows[2];
            assert!(
                added
                    .trim_start_matches('▎')
                    .trim_start()
                    .starts_with('\u{2502}'),
                "the old half holds no line there: {added:?}"
            );
            assert!(
                added.contains("+ for round in 0..STARTS {"),
                "the new line draws at the right: {added:?}"
            );
            let gap = app.diff_styles(Some(Role::SurfaceFill)).gap;
            assert_eq!(
                style(&app, 20, 6),
                gap.expect("a gap fill"),
                "the old half is the gap fill"
            );
        }

        /// A file whose extension names no language shows its counts alone.
        #[test]
        fn diff_unknown_extension_has_no_language() {
            let tool = finished(r#"{"path":"notes.txt","old_string":"one","new_string":"two"}"#);
            let mut app = app(true);
            paint_one(&mut app, &tool);
            let line = row(&app, 2);
            assert!(line.ends_with("+1 -1  \u{25b8}"), "{line}");
            assert_eq!(
                line.split_whitespace().count(),
                7,
                "no language sits between the path and the counts: {line}"
            );
        }

        #[test]
        fn turn_rule_text_comes_from_the_usage_event() {
            let mut app = app(true);
            let entries = [entry(
                1,
                Block::Rule(Usage {
                    seconds: 12,
                    tokens: 4_100,
                    cents: 3,
                }),
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 2, "a blank row and the rule");
            assert!(row(&app, 0).is_empty(), "one blank row before the rule");
            assert_eq!(row(&app, 1), "────── 12s · 4.1k tokens · $0.03");
            assert_eq!(style(&app, 0, 1), app.style(Role::Dim, Emphasis::Plain));
        }

        #[test]
        fn steer_row_shows_the_hint_on_the_panel_fill() {
            let mut app = app(true);
            let entries = [entry(
                1,
                Block::Steer {
                    text: "check the Linux run too",
                    queued: false,
                },
            )];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 1);
            let line = row(&app, 0);
            assert!(
                line.starts_with("↳ steer  check the Linux run too"),
                "{line}"
            );
            assert!(
                line.ends_with("lands after this tool · backspace takes it back"),
                "{line}"
            );
            assert_eq!(
                style(&app, COLUMNS - 2 * MARGIN - 1, 0),
                app.style(Role::PanelFill, Emphasis::Plain),
                "the row sits on the panel fill"
            );
            assert_eq!(
                style(&app, 0, 0),
                over(&mut app, Role::Accent, Emphasis::Bold, Role::PanelFill),
                "the `↳` is the accent over the panel fill"
            );
            assert_eq!(
                style(&app, 2, 0),
                over(&mut app, Role::Text, Emphasis::Bold, Role::PanelFill),
                "`steer` is bold over the panel fill"
            );
        }

        #[test]
        fn new_lines_row_shows_the_count_on_the_panel_fill() {
            let mut app = app(true);
            let entries = [entry(1, Block::NewLines { count: 12 })];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 1);
            let line = row(&app, 0);
            assert_eq!(line.trim_start(), "↓ 12 new lines  end jumps down");
            let x = line.len() as u16 - line.trim_start().len() as u16;
            assert!(x > 0, "the row is centered: {line:?}");
            assert_eq!(
                style(&app, 0, 0),
                app.style(Role::PanelFill, Emphasis::Plain),
                "the row sits on the panel fill"
            );
        }

        /// A call with no diff and no result: one row of its own.
        fn bare(name: &'static str, target: &'static str) -> ToolRow<'static> {
            ToolRow {
                name,
                target,
                state: State::Done,
                right: &[],
                diff: &[],
                error: None,
                output: None,
                earlier: 0,
            }
        }

        /// A run of tool rows keeps one blank row above it and one below, and two
        /// runs in a row share the one between them.
        #[test]
        fn air_two_tool_blocks_share_the_air() {
            let mut app = app(false);
            let entries = [
                entry(1, Block::Tool(bare("read", "a.rs"))),
                entry(2, Block::Tool(bare("grep", "paint"))),
            ];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(
                drawn, 5,
                "a blank row, a tool row, one blank row, a tool row and a blank row"
            );
            assert_eq!(row(&app, 0), "", "a blank row above the run");
            assert!(
                row(&app, 1).starts_with("→ Read a.rs"),
                "{:?}",
                row(&app, 1)
            );
            assert_eq!(row(&app, 2), "", "one blank row between the two runs");
            assert!(
                row(&app, 3).starts_with("✱ Grep \"paint\""),
                "{:?}",
                row(&app, 3)
            );
            assert_eq!(row(&app, 4), "", "a blank row below the run");
        }

        /// A thought ends with a blank row of its own, and the run under it takes
        /// no second one: one blank row separates any two blocks.
        #[test]
        fn air_after_a_thought_is_one_blank_row() {
            let mut app = app(false);
            let entries = [
                entry(
                    1,
                    Block::Thinking {
                        seconds: 4,
                        text: "",
                    },
                ),
                entry(2, Block::Tool(bare("read", "a.rs"))),
            ];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(
                drawn, 5,
                "a blank row, the thought, one blank row, the call and a blank row"
            );
            assert_eq!(row(&app, 0), "");
            assert!(
                row(&app, 1).starts_with("thought for 4s"),
                "{:?}",
                row(&app, 1)
            );
            assert_eq!(row(&app, 2), "", "one blank row under the thought");
            assert!(row(&app, 3).starts_with("→ Read"), "{:?}", row(&app, 3));
            assert_eq!(row(&app, 4), "");
        }

        /// Only the newest edit shows the lines of its diff: an older one keeps
        /// its row alone until the tab opens it, and then its whole diff.
        #[test]
        fn newest_diff_tab_opens_an_old_edit_whole() {
            let mut app = at_width(100);
            let mut folds = Folds::new();
            let older = folds.add();
            let newest = folds.add();
            folds.show_diff(newest);
            let edit = Edit::new();
            let entries = [
                entry(older, Block::Tool(edit.row())),
                entry(newest, Block::Tool(edit.row())),
            ];
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
            let rows = screen_rows(&app);
            assert_eq!(rows[0], "");
            assert!(
                rows[2].ends_with("\u{25b8}"),
                "the older edit keeps its title: {:?}",
                rows[2]
            );
            assert_eq!(rows[3], "▎", "the older card draws no body");
            assert_eq!(rows[4], "", "one blank row between the two blocks");
            assert!(rows[6].ends_with("\u{25b8}"), "{:?}", rows[6]);
            assert_eq!(
                rows.iter()
                    .filter(|row| row.contains("- let mut tui = Tui::start"))
                    .count(),
                1,
                "one diff line on the screen, of the newest edit: {}",
                rows.join("\n")
            );
            assert!(at(&rows, "5 more lines \u{b7} tab opens").is_some());

            folds.toggle(older);
            app.screen.clear();
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
            let rows = screen_rows(&app);
            assert_eq!(
                rows.iter()
                    .filter(|row| row.contains("- let mut tui = Tui::start"))
                    .count(),
                2,
                "the tab opened every line of the older diff: {}",
                rows.join("\n")
            );
            assert!(
                rows[2].ends_with("\u{25be}"),
                "the older edit is open: {:?}",
                rows[2]
            );
        }

        /// A folded run that holds the newest edit draws that edit's first lines
        /// and the count of them under its summary row.
        #[test]
        fn newest_diff_shows_under_a_folded_summary_row() {
            let mut app = app(false);
            let fold = app.folds.add();
            let mut edit = edited("src/a.rs", 5, 1);
            edit.fold = app.folds.add();
            let newest = edit.fold;
            let run = Group::new(
                vec![call("1", "bash", r#"{"command":"cargo test"}"#), edit],
                fold,
            );
            let mut folds = Folds::new();
            folds.show_diff(newest);
            let drawn =
                app.paint_blocks(0, 0, ROWS, &[entry(fold, Block::Group(run.row()))], &folds);
            assert_eq!(
                drawn, 9,
                "a blank row, the summary's padding row, its row, the three rows its \
                 four diff lines make, the count, the padding row under them and a \
                 blank row"
            );
            let rows = screen_rows(&app);
            assert_eq!(rows[0], "");
            assert!(
                rows[2].starts_with("\u{258e} \u{2713} bash 1 \u{b7} edit 1"),
                "{:?}",
                rows[2]
            );
            assert!(
                rows[3].contains("1 - old"),
                "the first diff line: {:?}",
                rows[3]
            );
            assert_eq!(
                at(&rows, "4 + new"),
                Some(5),
                "the fourth diff line of the newest edit: {}",
                rows.join("\n")
            );
            assert!(
                rows[6].ends_with("2 more lines \u{b7} tab opens"),
                "the count of the lines the tab opens: {:?}",
                rows[6]
            );
            assert_eq!(rows[8], "");

            // A newer edit takes the lines away from the run's row.
            folds.show_diff(newest + 1);
            app.screen.clear();
            let drawn =
                app.paint_blocks(0, 0, ROWS, &[entry(fold, Block::Group(run.row()))], &folds);
            assert_eq!(
                drawn, 5,
                "a blank row, the summary's padding row, its row, the padding row under \
                 it and a blank row"
            );
        }

        #[test]
        fn blocks_rows_counts_the_rows_without_drawing_them() {
            let mut app = app(true);
            let edit = Edit::new();
            let entries = [entry(1, Block::Tool(edit.row()))];
            let folds = Folds::new();
            let rows = app.blocks_rows(&entries, &folds);
            assert_eq!(
                rows,
                1 + 3 + 3 + 1 + 1 + 1,
                "a blank row, the card's three rows, the three rows of halves four \
                 diff lines make, the count, the card's last row and a blank row"
            );
            assert!(row(&app, 0).is_empty(), "the count drew nothing");
        }

        /// The item of a finished edit whose diff has `lines` lines, under the
        /// fold state's number `fold`.
        fn finished_edit(fold: u64, lines: usize, call: &str) -> Item {
            let mut new = String::new();
            for index in 0..lines {
                let _ = writeln!(new, "let added_{index} = {index};");
            }
            let arguments = serde_json::json!({
                "path": "src/main.rs",
                "old_string": "",
                "new_string": new,
            })
            .to_string();
            let mut tool = Tool::new(call, "edit");
            tool.set_call("edit", &arguments, COLUMNS);
            tool.set_result(0, None, COLUMNS);
            tool.finish();
            tool.fold = fold;
            Item::Tool(Box::new(tool))
        }

        /// The text of one stored line of the ring, trailing blanks trimmed.
        fn ring_line(app: &App, index: u64) -> String {
            let mut text: String = app
                .screen
                .transcript()
                .runs_of(index)
                .map(|(text, _)| text)
                .collect();
            while text.ends_with(' ') {
                text.pop();
            }
            text.chars().skip(usize::from(MARGIN)).collect()
        }

        /// An open run stored in the ring keeps the fill of its cards to the
        /// right edge: the cards hang from the tree line, so their fill starts
        /// at a column and not at the row's start, and a row with no text past
        /// the bar still ends on the fill.
        #[test]
        fn a_stored_open_run_keeps_its_cards_fill_to_the_edge() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    edited("src/a.rs", 0, 0),
                    call("2", "bash", r#"{"command":"cargo test"}"#),
                ],
                fold,
            );
            app.folds.toggle(fold);
            app.pending.push(Item::Group(Box::new(run)));
            app.commit();
            app.paint();
            let surface = app.style(Role::SurfaceFill, Emphasis::Plain);
            let last = app.screen.columns() - MARGIN - 1;
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let bash = at(&rows, "$ cargo test").expect("the bash card's title");
            for (row, what) in [
                (bash - 1, "the pad above the title"),
                (bash, "the title"),
                (bash + 1, "the pad under the title"),
                (bash + 2, "the body"),
            ] {
                let y = u16::try_from(row).expect("a row");
                assert_eq!(style(&app, last, y), surface, "{what}:\n{shown}");
                assert_eq!(style(&app, 5, y), surface, "{what} at 4:\n{shown}");
                assert_ne!(style(&app, 2, y), surface, "{what}'s lead:\n{shown}");
            }
        }

        /// An open run stored in the ring draws each member's branch whole: the
        /// line of a row goes in only when the row ends without the member's
        /// first text, so the branch is not cut behind it.
        #[test]
        fn a_stored_open_run_draws_each_branch_whole() {
            let mut app = app(true);
            let fold = app.folds.add();
            let run = Group::new(
                vec![
                    call("1", "read", r#"{"path":"src/x.rs"}"#),
                    call("2", "bash", r#"{"command":"cargo test"}"#),
                ],
                fold,
            );
            app.folds.toggle(fold);
            app.pending.push(Item::Group(Box::new(run)));
            app.commit();
            app.paint();
            let rows = screen_rows(&app);
            let shown = rows.join("\n");
            let read = at(&rows, "src/x.rs").expect("the read's row");
            assert!(rows[read].starts_with("├───"), "{shown}");
            let bash = at(&rows, "$ cargo test").expect("the bash card's title");
            assert!(rows[bash].starts_with("└───\u{258e}"), "{shown}");
            assert_eq!(rows[bash - 1], "│   \u{258e}", "the pad:\n{shown}");
        }

        #[test]
        fn tab_opens_a_finished_edit_and_folds_it_back_in_place() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.pending.push(finished_edit(fold, 14, "call-1"));
            app.commit();
            assert_eq!(
                app.screen.transcript().ring_len(),
                1 + 3 + DIFF_FOLD_LINES + 1 + 1 + 1,
                "a blank row, the card's three rows, four diff lines, the count, the \
                 card's last row and a blank row"
            );
            assert!(
                ring_line(&app, 8).ends_with("10 more lines · tab opens"),
                "the folded card says the tab opens it: {:?}",
                ring_line(&app, 8)
            );
            assert!(
                ring_line(&app, 2).ends_with("▸"),
                "the folded card is marked folded"
            );
            app.paint();
            assert!(
                at(&screen_rows(&app), "10 more lines · tab opens").is_some(),
                "the screen says the tab opens the block:\n{}",
                screen_rows(&app).join("\n")
            );

            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(
                app.screen.transcript().ring_len(),
                1 + 3 + 14 + 1 + 1,
                "the tab opens every diff line in place"
            );
            assert!(
                ring_line(&app, 2).ends_with("▾"),
                "the opened card is marked open"
            );
            assert!(
                !ring_lines(&app).join("\n").contains("more lines"),
                "the count row is gone: {:?}",
                ring_lines(&app)
            );
            app.paint();
            let drawn = screen_rows(&app);
            assert!(
                at(&drawn, "let added_0 = 0;").is_some(),
                "the first diff line draws:\n{}",
                drawn.join("\n")
            );
            assert!(
                at(&drawn, "let added_13 = 13;").is_some(),
                "the last diff line draws, so the frame shows every open line:\n{}",
                drawn.join("\n")
            );

            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(
                app.screen.transcript().ring_len(),
                1 + 3 + DIFF_FOLD_LINES + 1 + 1 + 1,
                "the second tab folds the block back"
            );
            assert!(
                ring_line(&app, 8).ends_with("10 more lines · tab opens"),
                "the count is back: {:?}",
                ring_line(&app, 8)
            );
        }

        #[test]
        fn ctrl_o_opens_every_finished_block_in_the_ring() {
            let mut app = app(true);
            let first = app.folds.add();
            app.pending.push(finished_edit(first, 14, "call-1"));
            let second = app.folds.add();
            app.pending.push(finished_edit(second, 14, "call-2"));
            app.commit();
            assert_eq!(
                app.screen.transcript().ring_len(),
                1 + 2 * (3 + DIFF_FOLD_LINES + 1 + 1) + 2,
                "two folded cards under one blank row, with one between them"
            );

            app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
            assert_eq!(
                app.screen.transcript().ring_len(),
                1 + 2 * (3 + 14 + 1) + 2,
                "Ctrl+O opened every finished block"
            );
            assert_eq!(app.screen.transcript().rendered_open(first), Some(true));
            assert_eq!(app.screen.transcript().rendered_open(second), Some(true));
            assert!(
                ring_line(&app, 21).ends_with("▾"),
                "the second block opened too: {:?}",
                ring_line(&app, 21)
            );
        }

        /// A finished block opens in the room of its own lines: the line below
        /// it moves down by the lines it gained, and the view keeps its top.
        #[test]
        fn a_finished_block_opens_in_place_and_the_view_keeps_its_top() {
            let mut app = app(true);
            let plain: Vec<String> = (0..50).map(|index| format!("line {index}")).collect();
            app.pending.push(Item::Plain {
                lines: plain,
                dim: false,
            });
            let fold = app.folds.add();
            app.pending.push(finished_edit(fold, 14, "call-1"));
            app.pending.push(Item::Plain {
                lines: vec!["the line below".to_owned()],
                dim: false,
            });
            app.commit();
            let folded = 50 + 1 + 3 + DIFF_FOLD_LINES + 1 + 1 + 1 + 1;
            assert_eq!(app.screen.transcript().ring_len(), folded);
            assert_eq!(
                ring_line(&app, 61),
                "the line below",
                "the line below the folded block"
            );

            app.screen.transcript_mut().scroll_up(u64::MAX);
            assert_eq!(app.screen.transcript().top(), 0, "the view is at the top");
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

            assert_eq!(
                app.screen.transcript().ring_len(),
                50 + 1 + 3 + 14 + 1 + 1 + 1,
                "the block gained the diff lines it left out or left the count"
            );
            assert_eq!(
                ring_line(&app, 70),
                "the line below",
                "the line below moved down by the lines the block gained"
            );
            assert_eq!(
                app.screen.transcript().top(),
                0,
                "the view keeps its top line"
            );
            assert_eq!(app.screen.transcript().block_at(50), Some(fold));
        }

        #[test]
        fn inline_commits_a_finished_block_folded_without_the_tab_hint() {
            let mut app = App::new(&Initialized::default(), (COLUMNS, ROWS), false);
            app.sidebar_on = Some(false);
            app.reflow();
            let fold = app.folds.add();
            app.pending.push(finished_edit(fold, 14, "call-1"));
            app.commit();
            assert_eq!(
                app.screen.transcript().ring_len(),
                0,
                "inline mode keeps no ring"
            );
            let mut out = Vec::new();
            app.screen.diff(&mut out);
            let written = String::from_utf8_lossy(&out);
            assert!(
                written.contains("10 more lines"),
                "the printed block says how many lines it left out: {written:?}"
            );
            assert!(
                !written.contains("tab opens"),
                "a line of the scrollback cannot be opened: {written:?}"
            );
            assert!(
                written.contains('▸'),
                "the printed block is marked folded: {written:?}"
            );
        }

        #[test]
        fn tool_row_interns_its_styles_once() {
            let mut app = app(true);
            let edit = Edit::new();
            let entries = [entry(1, Block::Tool(edit.row()))];
            let folds = Folds::new();
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
            let interned = app.screen.style_count();
            app.screen.clear();
            app.paint_blocks(0, 0, ROWS, &entries, &folds);
            assert_eq!(
                app.screen.style_count(),
                interned,
                "a warm frame interns no new style, so it builds none per cell"
            );
        }

        /// A settled bash call that printed `output`, under the fold state's
        /// number `fold`.
        fn settled_call(fold: u64, arguments: &str, output: &str) -> Item {
            let mut tool = Tool::new("call-1", "bash");
            tool.set_call("bash", arguments, COLUMNS);
            tool.set_output(output, COLUMNS);
            tool.finish();
            tool.fold = fold;
            Item::Tool(Box::new(tool))
        }

        /// The output of `count` lines, `line 1` up to `line count`.
        fn lines_of(count: usize) -> String {
            let mut text = String::new();
            for line in 1..=count {
                let _ = writeln!(text, "line {line}");
            }
            text
        }

        /// The row the card of a call draws its title on, read by needle.
        fn card_title(app: &App, title: &str) -> usize {
            let rows = screen_rows(app);
            at(&rows, title)
                .unwrap_or_else(|| panic!("{title} does not draw:\n{}", rows.join("\n")))
        }

        #[test]
        fn card_output_draws_the_last_10_lines_and_a_hint() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.pending
                .push(settled_call(fold, r#"{"command":"seq 40"}"#, &lines_of(40)));
            app.commit();
            app.paint();
            let title = card_title(&app, "$ seq 40");
            assert_eq!(
                row(&app, (title - 1) as u16),
                "▎",
                "a padding row sits over the title"
            );
            assert_eq!(
                row(&app, (title + 1) as u16),
                "▎",
                "a padding row sits under the title"
            );
            for (offset, line) in (31..=40).enumerate() {
                assert_eq!(
                    row(&app, (title + 2 + offset) as u16),
                    format!("▎ line {line}"),
                    "the card shows the last 10 lines of the tail, in order"
                );
            }
            assert_eq!(
                row(&app, (title + 12) as u16),
                "▎ 30 more lines · tab opens",
                "the dim row under the body counts the lines it leaves out"
            );
            assert_eq!(
                row(&app, (title + 13) as u16),
                "▎",
                "a padding row sits under the body"
            );
            let drawn = screen_rows(&app);
            assert!(
                !drawn.iter().any(|line| line == "▎ line 30"),
                "the card leaves the earlier lines out:\n{}",
                drawn.join("\n")
            );
            assert_eq!(
                app.screen.transcript().ring_len(),
                17,
                "a blank row, the card's three rows, ten lines and the hint, and a blank row"
            );
        }

        #[test]
        fn card_output_opens_to_the_whole_tail() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.pending
                .push(settled_call(fold, r#"{"command":"seq 15"}"#, &lines_of(15)));
            app.commit();
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.paint();
            let title = card_title(&app, "$ seq 15");
            for (offset, line) in (1..=15).enumerate() {
                assert_eq!(
                    row(&app, (title + 2 + offset) as u16),
                    format!("▎ line {line}"),
                    "an open card shows every line of the tail"
                );
            }
            assert_eq!(
                row(&app, (title + 17) as u16),
                "▎",
                "a padding row sits under the body"
            );
            let drawn = screen_rows(&app);
            assert!(
                !drawn.iter().any(|line| line.contains("more lines")),
                "an open card draws no hint row:\n{}",
                drawn.join("\n")
            );
            assert_eq!(
                app.screen.transcript().ring_len(),
                21,
                "a blank row, the card's three rows, fifteen lines and a blank row"
            );
        }

        #[test]
        fn card_output_of_a_short_output_has_no_hint() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.pending.push(settled_call(
                fold,
                r#"{"command":"cat notes"}"#,
                "one\ntwo\nthree\n",
            ));
            app.commit();
            app.paint();
            let title = card_title(&app, "$ cat notes");
            for (offset, line) in ["one", "two", "three"].iter().enumerate() {
                assert_eq!(
                    row(&app, (title + 2 + offset) as u16),
                    format!("▎ {line}"),
                    "a short output draws whole"
                );
            }
            assert_eq!(
                row(&app, (title + 5) as u16),
                "▎",
                "a padding row sits under the body"
            );
            let drawn = screen_rows(&app);
            assert!(
                !drawn.iter().any(|line| line.contains("more lines")),
                "a short output has no hint row:\n{}",
                drawn.join("\n")
            );
        }

        #[test]
        fn card_output_of_a_running_call_has_no_body() {
            let mut app = app(true);
            let mut tool = bare("bash", "cargo test");
            tool.state = State::Running;
            let entries = [entry(1, Block::Tool(tool))];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(
                drawn, 5,
                "a blank row, the card's padding and title, its padding row and a \
                 blank row"
            );
            assert_eq!(row(&app, 1), "▎", "a padding row over the title");
            let title = row(&app, 2);
            assert!(title.starts_with("▎ $ cargo test"), "{title}");
            assert!(
                title.ends_with('▸'),
                "the folded card marks itself: {title}"
            );
            assert_eq!(row(&app, 3), "▎", "a padding row under the title");
            assert_eq!(row(&app, 4), "", "a call with no output draws no body");
            assert_eq!(
                style(&app, 0, 3),
                over(&mut app, Role::Warn, Emphasis::Plain, Role::SurfaceFill),
                "the card ends after its last padding row"
            );
            assert_eq!(
                style(&app, 0, 2),
                over(&mut app, Role::Warn, Emphasis::Plain, Role::SurfaceFill),
                "the bar of a running call is the warning role"
            );
        }

        #[test]
        fn card_output_fills_every_row_with_the_surface_fill() {
            let mut app = app(true);
            let fold = app.folds.add();
            app.pending.push(settled_call(
                fold,
                r#"{"command":"cat notes"}"#,
                "one\ntwo\n",
            ));
            app.commit();
            app.paint();
            let bar = over(&mut app, Role::Line, Emphasis::Plain, Role::SurfaceFill);
            let mark = over(&mut app, Role::Dim, Emphasis::Plain, Role::SurfaceFill);
            let surface = app.style(Role::SurfaceFill, Emphasis::Plain);
            let title = card_title(&app, "$ cat notes");
            let last = usize::from(COLUMNS - MARGIN - 1);
            for y in title - 1..=title + 3 {
                let y = y as u16;
                assert_eq!(style(&app, 0, y), bar, "the bar runs down every row");
                for x in [1, last] {
                    assert_eq!(
                        style(&app, x as u16, y),
                        surface,
                        "row {y} fills the surface up to column {x}"
                    );
                }
            }
            assert_eq!(
                style(&app, 2, title as u16),
                mark,
                "the title's mark is dim over the surface"
            );
        }

        #[test]
        fn card_output_in_terminal_mode_has_the_bar_and_no_fill() {
            let mut app = app(false);
            assert_eq!(
                app.look.theme.fill(Role::SurfaceFill),
                Color::Default,
                "terminal mode draws no surface"
            );
            let mut tool = bare("bash", "cargo test");
            tool.output = Some("one\n");
            let entries = [entry(1, Block::Tool(tool))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert!(row(&app, 2).starts_with("▎ $ cargo test"));
            assert_eq!(row(&app, 4), "▎ one");
            assert_eq!(
                style(&app, 0, 2),
                app.style(Role::Line, Emphasis::Plain),
                "the bar keeps the line role and no fill"
            );
            for y in 1..=4 {
                for x in 0..COLUMNS {
                    assert!(
                        themed(&app, style(&app, x, y)),
                        "row {y} cell {x} is built off a role, so no cell draws a background"
                    );
                }
            }
        }

        #[test]
        fn card_output_draws_write_and_codemode_titles() {
            let mut app = app(true);
            let right = [Span {
                text: "written",
                role: SpanRole::Dim,
            }];
            let mut write = bare("write", "crates/tui/src/app/mod.rs");
            write.right = &right;
            write.output = Some("wrote 212 lines\n");
            let mut codemode = bare("codemode", "git log --oneline");
            codemode.output = Some("3 commits\n");
            let entries = [
                entry(1, Block::Tool(write)),
                entry(2, Block::Tool(codemode)),
            ];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let rows = screen_rows(&app);
            let write = at(&rows, "← Write").unwrap_or_else(|| panic!("{}", rows.join("\n")));
            assert!(
                rows[write].starts_with("▎ ← Write crates/tui/src/app/mod.rs"),
                "{:?}",
                rows[write]
            );
            assert!(
                rows[write].ends_with("written  ▸"),
                "the result sits at the right edge: {:?}",
                rows[write]
            );
            assert_eq!(rows[write + 2], "▎ wrote 212 lines");
            let codemode = at(&rows, "◆ Codemode").unwrap_or_else(|| panic!("{}", rows.join("\n")));
            assert!(
                rows[codemode].starts_with("▎ ◆ Codemode git log --oneline"),
                "{:?}",
                rows[codemode]
            );
            assert_eq!(rows[codemode + 2], "▎ 3 commits");
        }

        #[test]
        fn card_output_leaves_an_edit_and_a_failed_call_as_they_were() {
            let mut app = app(true);
            let edit = app.folds.add();
            app.pending.push(finished_edit(edit, 3, "call-1"));
            let failed = app.folds.add();
            let mut call = Tool::new("call-2", "bash");
            call.set_call("bash", r#"{"command":"cargo test"}"#, COLUMNS);
            call.set_result(12, Some("no such file"), COLUMNS);
            call.set_output("line 1\nline 2\n", COLUMNS);
            call.finish();
            call.fold = failed;
            app.pending.push(Item::Tool(Box::new(call)));
            app.commit();

            app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
            app.paint();
            let rows = screen_rows(&app);
            assert!(
                at(&rows, "let added_0 = 0;").is_some(),
                "an opened edit draws its diff:\n{}",
                rows.join("\n")
            );
            assert!(
                at(&rows, "no such file").is_some(),
                "an opened failed call draws its error:\n{}",
                rows.join("\n")
            );
            assert!(
                at(&rows, "line 1").is_none() && at(&rows, "line 2").is_none(),
                "neither draws output rows:\n{}",
                rows.join("\n")
            );
        }

        /// The rows of the card whose title the screen shows `title` on: its
        /// padding row, its title, and the `body` rows under the title.
        fn card_rows(app: &App, title: &str, body: usize) -> std::ops::RangeInclusive<u16> {
            let top = card_title(app, title);
            let first = u16::try_from(top - 1).expect("a row");
            first..=first + u16::try_from(body + 1).expect("a row")
        }

        #[test]
        fn card_edit_of_a_write_shows_its_content_as_a_diff() {
            let mut app = at_width(119);
            let edit = Edit::new();
            let mut call = edit.row();
            call.name = "write";
            call.output = Some("wrote 12 lines");
            let entries = [entry(1, Block::Tool(call))];
            app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            let title = row(&app, 2);
            assert!(
                title.starts_with("\u{258e} \u{2190} Write crates/cli/tests/tui_footprint.rs"),
                "the title is the write's mark and path: {title:?}"
            );
            assert!(
                row(&app, 5).contains("637 - let mut tui = Tui::start(&home, ARGS);"),
                "the write's body is its diff, not its output"
            );
        }

        #[test]
        fn card_edit_draws_its_title_and_diff_inside_the_card() {
            let mut app = at_width(119);
            let edit = Edit::new();
            let entries = [entry(1, Block::Tool(edit.row()))];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 11, "a blank row, the card's nine rows, a blank row");
            assert_eq!(row(&app, 1), "▎", "a padding row sits over the title");
            let title = row(&app, 2);
            assert!(
                title.starts_with("▎ ← Edit crates/cli/tests/tui_footprint.rs"),
                "the title is the mark and the path: {title:?}"
            );
            assert!(
                title.ends_with("rust  +14 -3  ▸"),
                "the result sits at the right edge: {title:?}"
            );
            assert_eq!(row(&app, 3), "▎", "a padding row sits under the title");
            let context = row(&app, 4);
            assert!(
                context.ends_with("let provider = home.script(\"one\", REPLY);"),
                "the first line of the diff draws in the card: {context:?}"
            );
            let removed = row(&app, 5);
            assert!(
                removed.contains("637 - let mut tui = Tui::start(&home, ARGS);"),
                "a removed line keeps its sign: {removed:?}"
            );
            let added = row(&app, 6);
            assert!(
                added.contains("637 + let mut start = Duration::MAX;"),
                "an added line keeps its sign: {added:?}"
            );
            assert_eq!(
                row(&app, 8),
                "▎ 5 more lines · tab opens",
                "the hint sits under the lines the card shows"
            );
            assert_eq!(row(&app, 9), "▎", "a padding row sits under the body");
            let bar = over(&mut app, Role::Line, Emphasis::Plain, Role::SurfaceFill);
            let surface = app.style(Role::SurfaceFill, Emphasis::Plain);
            let last = app.screen.columns() - MARGIN - 1;
            for y in 1..=9 {
                assert_eq!(style(&app, 0, y), bar, "row {y} starts with the bar");
                assert_eq!(
                    style(&app, last, y),
                    surface,
                    "row {y} fills the surface to the right margin"
                );
            }
            assert_eq!(
                style(&app, 2, 2),
                over(&mut app, Role::Dim, Emphasis::Plain, Role::SurfaceFill),
                "the mark of the title is dim over the surface"
            );
            let added = app.diff_line(Some(Role::SurfaceFill), DiffKind::Add);
            assert_eq!(
                style(&app, 10, 6),
                added.token(Token::Text),
                "an added line keeps its tint over the card"
            );
            let removed = app.diff_line(Some(Role::SurfaceFill), DiffKind::Delete);
            assert_eq!(
                style(&app, 10, 5),
                removed.token(Token::Text),
                "a removed line keeps its tint over the card"
            );
            assert_ne!(
                added.token(Token::Text),
                removed.token(Token::Text),
                "the two tints are their own"
            );
        }

        #[test]
        fn card_edit_only_the_newest_shows_its_diff() {
            let mut app = at_width(119);
            let older = Edit::new();
            let newer = Edit::new();
            let mut newer_row = newer.row();
            newer_row.target = "crates/tui/src/app/mod.rs";
            let entries = [
                entry(1, Block::Tool(older.row())),
                entry(2, Block::Tool(newer_row)),
            ];
            let mut folds = Folds::new();
            folds.show_diff(2);
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &folds);
            assert_eq!(
                drawn, 15,
                "a blank row, two cards, a blank row between them and a blank row"
            );
            let title = row(&app, 2);
            assert!(
                title.starts_with("▎ ← Edit crates/cli/tests/tui_footprint.rs"),
                "the older edit keeps its title: {title:?}"
            );
            assert_eq!(
                row(&app, 3),
                "▎",
                "the older edit draws no line and no hint"
            );
            let rows = screen_rows(&app);
            assert!(
                at(&rows, "let provider").is_none_or(|row| row > 4),
                "the older edit draws no diff line:\n{}",
                rows.join("\n")
            );
            let title = card_title(&app, "▎ ← Edit crates/tui/src/app/mod.rs");
            let first = row(&app, (title + 2) as u16);
            assert!(
                first.ends_with("let provider = home.script(\"one\", REPLY);"),
                "the newest edit draws its diff in its card: {first:?}"
            );
            assert_eq!(
                row(&app, (title + 6) as u16),
                "▎ 5 more lines · tab opens",
                "the newest edit counts the lines the tab opens"
            );
        }

        #[test]
        fn card_failed_bash_is_red_with_its_error() {
            let mut app = app(true);
            let fold = app.folds.add();
            let error = lines_of(20);
            let mut call = Tool::new("call-1", "bash");
            call.set_call(
                "bash",
                r#"{"command":"cargo nextest run -p reagent-tui"}"#,
                COLUMNS,
            );
            call.set_result(120, Some(&error), COLUMNS);
            call.finish();
            call.fold = fold;
            app.pending.push(Item::Tool(Box::new(call)));
            app.commit();
            app.paint();
            let bar = over(&mut app, Role::Error, Emphasis::Plain, Role::ErrFill);
            let fill = app.style(Role::ErrFill, Emphasis::Plain);
            let last = COLUMNS - MARGIN - 1;
            for y in card_rows(&app, "✗ $ cargo nextest run -p reagent-tui", 13) {
                assert_eq!(
                    style(&app, 0, y),
                    bar,
                    "row {y} has a bar in the error role"
                );
                assert_eq!(style(&app, 1, y), fill, "row {y} sits on the error tint");
                assert_eq!(
                    style(&app, last, y),
                    fill,
                    "row {y} fills the error tint to the right margin"
                );
            }
            let title = card_title(&app, "▎ ✗ $ cargo nextest run -p reagent-tui");
            let title_row = row(&app, title as u16);
            assert!(
                title_row.starts_with("▎ ✗ $ cargo nextest run -p reagent-tui"),
                "the cross sits before the title: {title_row:?}"
            );
            for (offset, line) in (11..=20).enumerate() {
                assert_eq!(
                    row(&app, (title + 2 + offset) as u16),
                    format!("▎ line {line}"),
                    "the body is the last 10 lines of the error"
                );
            }
            assert_eq!(
                row(&app, (title + 12) as u16),
                "▎ 10 more lines · tab opens",
                "the dim row counts the lines the tab opens"
            );
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.paint();
            let rows = screen_rows(&app);
            assert!(
                at(&rows, "line 1").is_some(),
                "the tab shows the whole error:\n{}",
                rows.join("\n")
            );
            assert!(
                at(&rows, "more lines").is_none(),
                "an open card counts nothing:\n{}",
                rows.join("\n")
            );
        }

        #[test]
        fn card_failed_edit_is_red_with_its_error() {
            let mut app = app(true);
            let edit = Edit::new();
            let failed = ToolRow {
                name: "edit",
                target: "crates/cli/tests/tui_footprint.rs",
                state: State::Failed,
                right: &EDIT_RIGHT,
                diff: &edit.diff,
                error: Some("the file is locked"),
                output: None,
                earlier: 0,
            };
            let entries = [entry(1, Block::Tool(failed))];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 7, "a blank row, the card's five rows, a blank row");
            let title = row(&app, 2);
            assert!(
                title.starts_with("▎ ✗ ← Edit crates/cli/tests/tui_footprint.rs"),
                "the cross sits before the title of the edit: {title:?}"
            );
            assert!(
                title.ends_with("rust  +14 -3  ▸"),
                "the result still sits at the right edge: {title:?}"
            );
            assert_eq!(
                row(&app, 4),
                "▎ the file is locked",
                "the body is the error"
            );
            assert_eq!(row(&app, 5), "▎", "a padding row sits under the body");
            let rows = screen_rows(&app);
            assert!(
                at(&rows, "let provider").is_none(),
                "a failed edit draws no diff line:\n{}",
                rows.join("\n")
            );
            let bar = over(&mut app, Role::Error, Emphasis::Plain, Role::ErrFill);
            let fill = app.style(Role::ErrFill, Emphasis::Plain);
            for y in 1..=5 {
                assert_eq!(
                    style(&app, 0, y),
                    bar,
                    "row {y} has a bar in the error role"
                );
                assert_eq!(style(&app, 1, y), fill, "row {y} sits on the error tint");
            }
        }

        #[test]
        fn card_failed_in_terminal_mode_has_the_red_bar_and_no_fill() {
            let mut app = app(false);
            let failed = ToolRow {
                name: "bash",
                target: "cargo test",
                state: State::Failed,
                right: &[],
                diff: &[],
                error: Some("the file is locked"),
                output: None,
                earlier: 0,
            };
            let entries = [entry(1, Block::Tool(failed))];
            let drawn = app.paint_blocks(0, 0, ROWS, &entries, &Folds::new());
            assert_eq!(drawn, 7, "a blank row, the card's five rows, a blank row");
            let bar = app.style(Role::Error, Emphasis::Plain);
            for y in 1..=5 {
                assert_eq!(
                    style(&app, 0, y),
                    bar,
                    "row {y} has a bar in the error role, with no fill behind it"
                );
                for x in 0..COLUMNS {
                    assert!(
                        themed(&app, style(&app, x, y)),
                        "row {y} draws no fill, cell {x} has one"
                    );
                }
            }
            assert_eq!(
                row(&app, 4),
                "▎ the file is locked",
                "the body is the error"
            );
        }

        #[test]
        fn card_output_cuts_a_long_line_to_the_width() {
            let mut app = app(true);
            let fold = app.folds.add();
            let output = format!("short\n{}\n", "a".repeat(300));
            app.pending
                .push(settled_call(fold, r#"{"command":"cat wide"}"#, &output));
            app.commit();
            app.paint();
            let title = card_title(&app, "$ cat wide");
            let width = usize::from(COLUMNS - 2 * MARGIN);
            assert_eq!(
                row(&app, (title + 3) as u16),
                format!("▎ {}", "a".repeat(width - 2)),
                "the card cuts its widest line at the width of the block"
            );
            let drawn = screen_rows(&app);
            assert!(
                drawn[title + 4] == "▎",
                "the line is cut, not wrapped:\n{}",
                drawn.join("\n")
            );
            assert_eq!(
                app.screen.transcript().ring_len(),
                1 + 3 + 2 + 1 + 1,
                "a long line takes one row, not one row per piece of it"
            );
        }
    }
}
