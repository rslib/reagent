//! The readline state of the text box: the kill ring, the undo log, the command
//! the last key ran, the argument the digit keys typed, the mark, and the key a
//! quoted insert or a character search waits for, and the incremental search
//! over the history.
//!
//! Every command lives here and every primitive it needs is in the editor: the
//! word edges over the gap buffer's [`Scan`], the transposes, the case commands
//! and the range replacements.
//!
//! A kill moves its text into the ring and the undo step that takes the kill
//! back shares that entry, so the killed bytes are held once; the chips the kill
//! covered ride on the entry, so a yank of it puts them back whole. A character
//! typed after another one grows the last undo step in place and allocates
//! nothing; every other command starts a step of its own. A step keeps the folded
//! chips the edit covered, so an undo puts a chip back whole and never leaves the
//! mark that stands for one in the text.

use std::collections::VecDeque;
use std::sync::Arc;

use super::{Case, Editor, Paste};

/// The most kills the ring keeps.
const RING: usize = 10;

/// The most steps the undo log keeps: the oldest goes, and the text it took
/// becomes the buffer the next step writes into, so an erase costs no heap block
/// once the log is full and a step has a buffer to give.
const LIMIT: usize = 128;

/// The largest the argument the digit keys type grows to. Readline caps it the
/// same way, so a long run of digits can neither overflow it nor run a command
/// for ever.
const ARG_MAX: i32 = 1_000_000;

/// One kill: the bytes it took and the folded chips that stood among them.
///
/// The ring entry and the undo step that takes the kill back share it, so the
/// killed bytes are held once, and a yank of the entry puts the chips back
/// whole instead of leaving the marks that stood for them.
#[derive(Debug)]
pub(super) struct Kill {
    /// The killed bytes, with a folded chip's mark where the chip stood.
    text: String,
    /// The chips the kill took, at their offset into `text`.
    chips: Vec<(usize, Paste)>,
}

impl Kill {
    /// The bytes of the entry in `from..to`.
    fn slice(&self, (from, to): (usize, usize)) -> &str {
        &self.text[from..to]
    }
}

/// The text one undo step takes out, and where it is kept.
#[derive(Debug)]
pub(super) enum Removed {
    /// Text nothing else holds.
    Text(String),
    /// The ring entry a kill moved the text into, and the bytes in it.
    Ring(Arc<Kill>, (usize, usize)),
}

impl Removed {
    /// The text the step takes back.
    fn text(&self) -> &str {
        match self {
            Removed::Text(text) => text,
            Removed::Ring(entry, range) => entry.slice(*range),
        }
    }

    /// How many bytes the step took out.
    fn len(&self) -> usize {
        self.text().len()
    }
}

/// An incremental search over the history, as Ctrl+R and Ctrl+S open it.
///
/// The match is the text box's own text while the search is open, so the box
/// draws the entry and the cursor sits at the match; the draft the search
/// started from is kept here and [`Editor::search_abort`] puts it back.
#[derive(Debug)]
pub(super) struct Search {
    /// Whether it looks at older entries (Ctrl+R) or newer ones (Ctrl+S).
    forward: bool,
    /// The query typed into it.
    query: String,
    /// The entry of the history the match is in.
    entry: usize,
    /// The byte offset of the match in that entry.
    at: usize,
    /// Whether the query matched nothing, so the prompt says it failed.
    failed: bool,
    /// The text box as it was before the search, and its cursor.
    draft: String,
    cursor: usize,
    /// The chips of the draft, which the search's text has none of.
    pastes: Vec<Paste>,
    /// The prompt, rebuilt when the state changes so a frame borrows it.
    prompt: String,
}

impl Search {
    /// Write the prompt the box draws on its first row, `(reverse-i-search)`
    /// over the query, or the failed form that names the last match shown.
    fn prompt(&mut self) {
        let name = match (self.failed, self.forward) {
            (true, true) => "(failed i-search)",
            (true, false) => "(failed reverse-i-search)",
            (false, true) => "(i-search)",
            (false, false) => "(reverse-i-search)",
        };
        self.prompt.clear();
        self.prompt.push_str(name);
        self.prompt.push('`');
        self.prompt.push_str(&self.query);
        self.prompt.push_str("': ");
    }
}

/// A match an incremental search found: the entry of the history and the byte
/// offset of the query in it.
#[derive(Debug, Clone, Copy)]
struct Found {
    /// The index of the entry in the history.
    entry: usize,
    /// The byte offset of the match in that entry.
    at: usize,
}

/// One step of the undo log.
#[derive(Debug)]
struct Step {
    /// Where the edit started.
    at: usize,
    /// What it took out.
    removed: Removed,
    /// The bytes it added at `at`.
    added: usize,
    /// The folded chips it took out, at their offset from `at`.
    chips: Vec<(usize, Paste)>,
    /// Where the cursor was before the command, when its undo puts the cursor
    /// back there rather than past the text it puts back.
    point: Option<usize>,
}

/// The command the last key ran: it joins the kills in a row and lets a
/// yank-pop follow only a yank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Last {
    /// A typed character.
    Type,
    /// A kill forward, or backward.
    Kill,
    /// A yank or a yank-pop.
    Yank,
    /// Any other command.
    Other,
}

/// The readline state of the text box.
#[derive(Debug)]
pub(super) struct Readline {
    /// The kills, the newest at the front.
    ring: VecDeque<Arc<Kill>>,
    /// The ring entry the last yank took, for the yank-pop.
    yank: Option<usize>,
    /// The bytes the last yank put in the text, which a yank-pop takes out.
    yanked: usize,
    /// The undo log, oldest first.
    undo: Vec<Step>,
    /// The buffer the removed text of the next step is written into: the text
    /// of a step the log dropped, reused.
    spare: String,
    /// The command the last key ran.
    last: Last,
    /// The argument the digit keys typed: 0 when none has.
    arg: i32,
    /// Whether a digit key started the argument.
    digits: bool,
    /// Where Ctrl+Space set the mark.
    mark: Option<usize>,
    /// Whether the next key is inserted as it is.
    quote: bool,
    /// Whether a character search waits for its character, and looks forward.
    char_search: Option<bool>,
    /// The incremental search over the history, open until a key ends it.
    search: Option<Search>,
    /// The entry the last Alt+. walked to and the bytes it inserted, which the
    /// press after it replaces.
    last_arg: Option<(usize, usize)>,
}

impl Default for Readline {
    fn default() -> Self {
        Readline {
            ring: VecDeque::new(),
            yank: None,
            yanked: 0,
            undo: Vec::with_capacity(LIMIT),
            spare: String::new(),
            last: Last::Other,
            arg: 0,
            digits: false,
            mark: None,
            quote: false,
            char_search: None,
            search: None,
            last_arg: None,
        }
    }
}

impl Readline {
    /// Record `len` bytes inserted at `at`, as one step of its own.
    pub(super) fn inserted(&mut self, at: usize, len: usize) {
        self.push(at, Removed::Text(String::new()), len, Vec::new());
        self.last = Last::Other;
        self.last_arg = None;
    }

    /// Record a typed character at `at`: characters typed in a row are one
    /// step, so the step grows in place, unless a move ended the run.
    pub(super) fn typed(&mut self, at: usize, len: usize) {
        let grows = matches!(
            (self.last, self.undo.last()),
            (Last::Type, Some(step)) if step.at + step.added == at
        );
        if grows {
            if let Some(step) = self.undo.last_mut() {
                step.added += len;
            }
            self.shift(at, 0, len);
        } else {
            self.push(at, Removed::Text(String::new()), len, Vec::new());
        }
        self.last = Last::Type;
        self.last_arg = None;
    }

    /// Drop the undo log and the mark: a chip that opened or folded moved the
    /// text under the positions they hold.
    pub(super) fn forget(&mut self) {
        self.undo.clear();
        self.mark = None;
    }

    /// The whole text was swapped for another one: the undo log and the mark
    /// point into the text that went, and the walk of Alt+. is over.
    pub(super) fn swapped(&mut self) {
        self.forget();
        self.last = Last::Other;
        self.last_arg = None;
    }

    /// Empty the state that points into the text, keeping the kill ring.
    pub(super) fn clear(&mut self) {
        self.swapped();
        self.arg = 0;
        self.digits = false;
        self.quote = false;
        self.char_search = None;
        self.search = None;
    }

    /// Make room for one more step: a full log drops its oldest, and the text it
    /// took becomes the buffer the next step writes its own into.
    pub(super) fn room(&mut self) {
        while self.undo.len() >= LIMIT {
            let old = self.undo.remove(0);
            if let Removed::Text(mut text) = old.removed {
                text.clear();
                if text.capacity() > self.spare.capacity() {
                    std::mem::swap(&mut self.spare, &mut text);
                }
            }
        }
    }

    /// One step of the undo log, with the mark moved with the text.
    fn push(&mut self, at: usize, removed: Removed, added: usize, chips: Vec<(usize, Paste)>) {
        self.room();
        let taken = removed.len();
        self.shift(at, taken, added);
        self.undo.push(Step {
            at,
            removed,
            added,
            chips,
            point: None,
        });
    }

    /// Move the mark past an edit at `at` that took `removed` bytes out and put
    /// `added` of them in, so that it never lands inside a character.
    fn shift(&mut self, at: usize, removed: usize, added: usize) {
        if let Some(mark) = self.mark {
            self.mark = Some(if mark >= at + removed {
                mark - removed + added
            } else if mark >= at {
                at
            } else {
                mark
            });
        }
    }

    /// Record where the cursor was before the command the last step is: its undo
    /// puts the cursor back there.
    pub(super) fn point(&mut self, at: usize) {
        if let Some(step) = self.undo.last_mut() {
            step.point = Some(at);
        }
    }

    /// The argument the digit keys typed, taking it: the command that reads it
    /// is no kill and no yank.
    fn take(&mut self) -> i32 {
        let arg = if self.digits {
            self.arg
        } else if self.arg < 0 {
            // A sign with no digit after it is an argument of -1.
            -1
        } else {
            1
        };
        self.arg = 0;
        self.digits = false;
        self.last = Last::Other;
        self.last_arg = None;
        arg
    }

    /// Whether the last command was a typed character, which the next one joins.
    fn typing(&self) -> bool {
        self.last == Last::Type
    }

    /// A move ends a run of typed characters and a run of kills.
    pub(super) fn moved(&mut self) {
        self.last = Last::Other;
        self.last_arg = None;
    }

    /// Whether the last command was a kill, which a kill in a row joins.
    fn killing(&self) -> bool {
        self.last == Last::Kill
    }

    /// Whether the last command was a yank, which a yank-pop follows.
    fn yanking(&self) -> bool {
        self.last == Last::Yank
    }

    /// Put `text` and the chips it held on the ring and answer the entry and the
    /// bytes it took: a kill in a row replaces the newest entry, appended when it
    /// goes forward and prepended when it goes back, so the ring keeps one entry
    /// for the kills of a run.
    fn kill(
        &mut self,
        text: String,
        chips: &[(usize, Paste)],
        back: bool,
        join: bool,
    ) -> (Arc<Kill>, (usize, usize)) {
        let old = if join {
            self.ring.front().map(Arc::clone)
        } else {
            None
        };
        let (entry, range) = match old {
            Some(old) => {
                let mut joined = String::with_capacity(old.text.len() + text.len());
                let mut held = Vec::with_capacity(old.chips.len() + chips.len());
                let range = if back {
                    joined.push_str(&text);
                    joined.push_str(&old.text);
                    held.extend(chips.iter().map(|(at, chip)| (*at, chip.clone())));
                    held.extend(
                        old.chips
                            .iter()
                            .map(|(at, chip)| (at + text.len(), chip.clone())),
                    );
                    (0, text.len())
                } else {
                    joined.push_str(&old.text);
                    joined.push_str(&text);
                    held.extend(old.chips.iter().map(|(at, chip)| (*at, chip.clone())));
                    held.extend(
                        chips
                            .iter()
                            .map(|(at, chip)| (at + old.text.len(), chip.clone())),
                    );
                    (old.text.len(), old.text.len() + text.len())
                };
                (
                    Arc::new(Kill {
                        text: joined,
                        chips: held,
                    }),
                    range,
                )
            }
            None => {
                let len = text.len();
                (
                    Arc::new(Kill {
                        text,
                        chips: chips.to_vec(),
                    }),
                    (0, len),
                )
            }
        };
        if join {
            self.ring.pop_front();
        }
        self.ring.push_front(Arc::clone(&entry));
        if self.ring.len() > RING {
            self.ring.pop_back();
        }
        self.last = Last::Kill;
        (entry, range)
    }

    /// The kill Ctrl+Y puts back, and the entry a yank-pop walks from.
    fn yank(&mut self) -> Option<Arc<Kill>> {
        let entry = self.ring.front().map(Arc::clone)?;
        self.yank = Some(0);
        Some(entry)
    }

    /// The entry Alt+Y puts in place of the yanked text.
    fn yank_pop(&mut self) -> Option<Arc<Kill>> {
        let index = self.yank?;
        if self.ring.is_empty() {
            return None;
        }
        let next = (index + 1) % self.ring.len();
        self.yank = Some(next);
        Some(Arc::clone(&self.ring[next]))
    }
}

impl Editor {
    /// Ctrl+F: one character forward, the argument's count of them.
    pub fn forward_char(&mut self) {
        let count = self.readline.take();
        for _ in 0..count.unsigned_abs() {
            if count > 0 {
                self.right();
            } else {
                self.left();
            }
        }
    }

    /// Ctrl+B: one character back, the argument's count of them.
    pub fn backward_char(&mut self) {
        let count = self.readline.take();
        for _ in 0..count.unsigned_abs() {
            if count > 0 {
                self.left();
            } else {
                self.right();
            }
        }
    }

    /// Alt+F, Ctrl+Right and Alt+Right: to the end of the word after the
    /// cursor, the argument's count of words.
    pub fn forward_word(&mut self) {
        let count = self.readline.take();
        for _ in 0..count.unsigned_abs() {
            let at = if count > 0 {
                self.word_after(self.gap.cursor()).1
            } else {
                self.word_before(self.gap.cursor()).0
            };
            self.gap.move_to(at);
        }
    }

    /// Alt+B, Ctrl+Left and Alt+Left: to the start of the word before the
    /// cursor, the argument's count of words.
    pub fn backward_word(&mut self) {
        let count = self.readline.take();
        for _ in 0..count.unsigned_abs() {
            let at = if count > 0 {
                self.word_before(self.gap.cursor()).0
            } else {
                self.word_after(self.gap.cursor()).1
            };
            self.gap.move_to(at);
        }
    }

    /// Ctrl+A: the start of the line.
    pub fn beginning_of_line(&mut self) {
        self.readline.take();
        self.line_start();
    }

    /// Ctrl+E: the end of the line.
    pub fn end_of_line(&mut self) {
        self.readline.take();
        self.line_end();
    }

    /// Backspace: remove the character before the cursor, the argument's count
    /// of them; a negative argument removes them forward.
    pub fn backward_delete_char(&mut self) {
        let count = self.readline.take();
        for _ in 0..count.unsigned_abs() {
            if count > 0 {
                self.backspace();
            } else {
                self.delete();
            }
        }
    }

    /// Delete and Ctrl+D: remove the character after the cursor, the argument's
    /// count of them; a negative argument removes them backward.
    pub fn delete_char(&mut self) {
        let count = self.readline.take();
        for _ in 0..count.unsigned_abs() {
            if count > 0 {
                self.delete();
            } else {
                self.backspace();
            }
        }
    }

    /// Ctrl+K: kill to the end of the line, the argument's count of lines; a
    /// negative argument kills back to the start of the line.
    pub fn kill_line(&mut self) {
        let joined = self.readline.killing();
        let count = self.readline.take();
        let (from, to) = if count >= 0 {
            let from = self.gap.cursor();
            (
                from,
                self.lines_forward(from, count.unsigned_abs() as usize),
            )
        } else {
            let to = self.gap.cursor();
            (self.lines_back(to, count.unsigned_abs() as usize), to)
        };
        self.kill_range(from, to, count < 0, joined);
    }

    /// Ctrl+U: kill back to the start of the line, the argument's count of
    /// lines; a negative argument kills forward.
    pub fn unix_line_discard(&mut self) {
        let joined = self.readline.killing();
        let count = self.readline.take();
        let (from, to) = if count >= 0 {
            let to = self.gap.cursor();
            (self.lines_back(to, count.unsigned_abs() as usize), to)
        } else {
            let from = self.gap.cursor();
            (
                from,
                self.lines_forward(from, count.unsigned_abs() as usize),
            )
        };
        self.kill_range(from, to, count >= 0, joined);
    }

    /// Ctrl+W: kill the whitespace and the word before the cursor, the
    /// argument's count of words; a negative argument kills them forward.
    pub fn unix_word_rubout(&mut self) {
        let joined = self.readline.killing();
        let count = self.readline.take();
        for i in 0..count.unsigned_abs() {
            let join = joined || i > 0;
            let cursor = self.gap.cursor();
            if count > 0 {
                let at = self.space_word_start(cursor);
                self.kill_range(at, cursor, true, join);
            } else {
                let at = self.space_word_end(cursor);
                self.kill_range(cursor, at, false, join);
            }
        }
    }

    /// Alt+D: kill the word after the cursor, the argument's count of them; a
    /// negative argument kills them backward.
    pub fn kill_word(&mut self) {
        let joined = self.readline.killing();
        let count = self.readline.take();
        for i in 0..count.unsigned_abs() {
            let join = joined || i > 0;
            let cursor = self.gap.cursor();
            if count > 0 {
                let at = self.word_after(cursor).1;
                self.kill_range(cursor, at, false, join);
            } else {
                let at = self.word_before(cursor).0;
                self.kill_range(at, cursor, true, join);
            }
        }
    }

    /// Alt+Backspace and Alt+Ctrl+H: kill the word before the cursor, the
    /// argument's count of them; a negative argument kills them forward.
    pub fn backward_kill_word(&mut self) {
        let joined = self.readline.killing();
        let count = self.readline.take();
        for i in 0..count.unsigned_abs() {
            let join = joined || i > 0;
            let cursor = self.gap.cursor();
            if count > 0 {
                let at = self.word_before(cursor).0;
                self.kill_range(at, cursor, true, join);
            } else {
                let at = self.word_after(cursor).1;
                self.kill_range(cursor, at, false, join);
            }
        }
    }

    /// Alt+\\: delete the spaces and tabs around the cursor. It is no kill.
    pub fn delete_horizontal_space(&mut self) {
        self.readline.take();
        let (from, to) = self.spaces();
        if from < to {
            self.splice(from, to, "");
        }
    }

    /// Ctrl+T: swap the character before the cursor with the one after it, or
    /// the two before it when the cursor ends the text.
    pub fn transpose_chars(&mut self) {
        self.readline.take();
        self.swap_graphemes();
    }

    /// Alt+T: swap the word before the cursor with the word after it.
    pub fn transpose_words(&mut self) {
        self.readline.take();
        self.swap_words();
    }

    /// Alt+U: upcase the word after the cursor, the argument's count of words.
    pub fn upcase_word(&mut self) {
        self.reword_argument(Case::Upper);
    }

    /// Alt+L: downcase the word after the cursor, the argument's count.
    pub fn downcase_word(&mut self) {
        self.reword_argument(Case::Lower);
    }

    /// Alt+C: capitalize the word after the cursor, the argument's count.
    pub fn capitalize_word(&mut self) {
        self.reword_argument(Case::Capitalize);
    }

    /// The case commands: one word each time, the argument's count of words.
    fn reword_argument(&mut self, case: Case) {
        let count = self.readline.take();
        for _ in 0..count.max(0) {
            if !self.reword(case) {
                break;
            }
        }
    }

    /// Ctrl+Y: put the newest kill back at the cursor, with the chips it held.
    pub fn yank(&mut self) {
        let count = self.readline.take();
        let Some(entry) = self.readline.yank() else {
            return;
        };
        for _ in 0..count.unsigned_abs() {
            let at = self.gap.cursor();
            self.splice(at, at, &entry.text);
            self.put_chips(at, &entry.chips);
        }
        self.readline.yanked = entry.text.len();
        self.readline.last = Last::Yank;
    }

    /// Alt+Y: replace the text the last yank put in with the kill before it.
    pub fn yank_pop(&mut self) {
        if !self.readline.yanking() {
            self.readline.take();
            return;
        }
        self.readline.take();
        let Some(entry) = self.readline.yank_pop() else {
            return;
        };
        let end = self.gap.cursor();
        let start = end.saturating_sub(self.readline.yanked);
        self.splice(start, end, &entry.text);
        self.put_chips(start, &entry.chips);
        self.readline.yanked = entry.text.len();
        self.readline.last = Last::Yank;
    }

    /// Ctrl+_ and Ctrl+X Ctrl+U: take the last edit back.
    pub fn undo(&mut self) {
        self.readline.take();
        self.undo_step();
    }

    /// Ctrl+Space: set the mark where the cursor is.
    pub fn set_mark(&mut self) {
        self.readline.take();
        self.readline.mark = Some(self.gap.cursor());
    }

    /// Ctrl+X Ctrl+X: put the cursor at the mark and the mark at the cursor.
    pub fn exchange_point_and_mark(&mut self) {
        self.readline.take();
        let cursor = self.gap.cursor();
        let Some(mark) = self.readline.mark.replace(cursor) else {
            return;
        };
        let at = self.boundary(mark);
        self.gap.move_to(at);
    }

    /// Ctrl+Q and Ctrl+V: insert the next key as it is.
    pub fn quote_next(&mut self) {
        self.readline.take();
        self.readline.quote = true;
    }

    /// Ctrl+] and Alt+Ctrl+]: wait for a character and move to it.
    pub fn search_char(&mut self, forward: bool) {
        self.readline.take();
        self.readline.char_search = Some(forward);
    }

    /// Whether the last key waits for the next one: a quoted insert or a
    /// character search.
    pub fn wants_key(&self) -> bool {
        self.readline.quote || self.readline.char_search.is_some()
    }

    /// The key a quoted insert or a character search waited for.
    pub fn next_key(&mut self, key: Option<char>) {
        let Some(key) = key else {
            self.readline.quote = false;
            self.readline.char_search = None;
            return;
        };
        if self.readline.quote {
            self.readline.quote = false;
            self.typed_key(key);
            return;
        }
        if let Some(forward) = self.readline.char_search.take()
            && let Some(at) = self.find_char(key, forward)
        {
            self.gap.move_to(at);
        }
        self.readline.last = Last::Other;
    }

    /// Alt+.: the last word of the newest message, and the last word of the
    /// message before it on each press after, replacing the one it put in.
    pub fn yank_last_arg<S: AsRef<str>>(&mut self, history: &[S]) {
        let previous = self.readline.last_arg;
        self.readline.take();
        let step = previous.map_or(0, |(step, _)| step + 1);
        let Some(entry) = history.len().checked_sub(step + 1) else {
            return;
        };
        let word = history[entry]
            .as_ref()
            .split_whitespace()
            .next_back()
            .unwrap_or("");
        let cursor = self.gap.cursor();
        let at = match previous {
            Some((_, inserted)) => cursor.saturating_sub(inserted),
            None => cursor,
        };
        if !word.is_empty() || at != cursor {
            self.splice(at, cursor, word);
        }
        self.readline.last_arg = Some((step, word.len()));
    }

    /// Ctrl+R and Ctrl+S: search the history back or forward, and find the next
    /// older or newer match while a search is open.
    pub fn search_history<S: AsRef<str>>(&mut self, forward: bool, history: &[S]) {
        self.readline.take();
        if self.readline.search.is_none() {
            self.search_begin(forward, history);
            return;
        }
        let Some(search) = self.readline.search.as_mut() else {
            return;
        };
        search.forward = forward;
        // A repeat takes the next match of the entry it shows before the one of
        // another entry: readline steps on from the match it landed on.
        let (entry, at, query) = (search.entry, search.at, search.query.as_str());
        let same = match history.get(entry) {
            Some(line) if forward => find_after(line.as_ref(), query, at),
            Some(line) => find_before(line.as_ref(), query, at),
            None => None,
        };
        if let Some(at) = same {
            self.search_found(history, Found { entry, at });
            return;
        }
        let step = if forward {
            entry.checked_add(1)
        } else {
            entry.checked_sub(1)
        };
        match step {
            Some(step) if step < history.len() => {
                self.search_run(history, step, None, forward, true);
            }
            _ => self.search_failed(),
        }
    }

    /// A printable key of the search: add it to the query and find the match
    /// again from the one it shows.
    pub fn search_push<S: AsRef<str>>(&mut self, c: char, history: &[S]) {
        let Some(search) = self.readline.search.as_mut() else {
            return;
        };
        search.query.push(c);
        let (start, bound, forward) = (search.entry, Some(search.at), search.forward);
        self.search_run(history, start, bound, forward, true);
    }

    /// Backspace in the search: shorten the query and look again from the end
    /// the direction comes from.
    pub fn search_pop<S: AsRef<str>>(&mut self, history: &[S]) {
        let Some(search) = self.readline.search.as_mut() else {
            return;
        };
        if search.query.pop().is_none() {
            return;
        }
        self.search_restart(history);
    }

    /// Ctrl+G: end the search and give the draft and the cursor it had back.
    pub fn search_abort(&mut self) {
        let Some(search) = self.readline.search.take() else {
            return;
        };
        self.gap.clear();
        self.gap.insert(&search.draft);
        self.pastes = search.pastes;
        let at = search.cursor.min(self.gap.len());
        self.gap.move_to(at);
        // The whole text is another one, so every row of the box is stale and
        // the draft the key was typing is still the history's to give back.
        self.readline.swapped();
        self.drop_rows(0);
    }

    /// Esc, Enter and every other key: end the search and keep the match.
    pub fn search_end(&mut self) {
        self.readline.search = None;
    }

    /// Whether an incremental search over the history is open.
    pub fn searching(&self) -> bool {
        self.readline.search.is_some()
    }

    /// The prompt the box draws on its first row while a search is open.
    pub fn search_prompt(&self) -> Option<&str> {
        self.readline
            .search
            .as_ref()
            .map(|search| search.prompt.as_str())
    }

    /// Open the search on the draft, showing the entry its direction comes from
    /// with the cursor at that entry's end. A history with no entry leaves the
    /// draft in the box and the prompt says the query failed.
    fn search_begin<S: AsRef<str>>(&mut self, forward: bool, history: &[S]) {
        let cursor = self.gap.cursor();
        let mut draft = String::with_capacity(self.gap.len());
        let (before, after) = self.gap.chunks();
        draft.push_str(before);
        draft.push_str(after);
        let pastes = self.pastes.clone();
        let entry = history.len().saturating_sub(1);
        let found = history.last().map(|line| Found {
            entry,
            at: line.as_ref().len(),
        });
        let mut search = Search {
            forward,
            query: String::new(),
            entry,
            at: found.map_or(0, |found| found.at),
            failed: found.is_none(),
            draft,
            cursor,
            pastes,
            prompt: String::new(),
        };
        search.prompt();
        self.readline.search = Some(search);
        if let Some(found) = found {
            self.search_show(history, found);
        }
    }

    /// Find the match again from the end the direction comes from: a shorter
    /// query can match anywhere again.
    fn search_restart<S: AsRef<str>>(&mut self, history: &[S]) {
        let Some(search) = self.readline.search.as_ref() else {
            return;
        };
        let (forward, empty) = (search.forward, search.query.is_empty());
        let Some(end) = history.len().checked_sub(1) else {
            // No message to search: the prompt follows the query and the text
            // keeps the draft.
            if let Some(search) = self.readline.search.as_mut() {
                search.prompt();
            }
            return;
        };
        let start = if forward { 0 } else { end };
        if !empty {
            self.search_run(history, start, None, forward, true);
            return;
        }
        let found = Found {
            entry: start,
            at: history[start].as_ref().len(),
        };
        self.search_found(history, found);
    }

    /// Find the match again from entry `start` and show it, where a bound limits
    /// that entry to a match at or before it. `strict` marks a query that found
    /// nothing at all, which the prompt says; a step that runs out of entries
    /// keeps the match it shows.
    fn search_run<S: AsRef<str>>(
        &mut self,
        history: &[S],
        start: usize,
        bound: Option<usize>,
        forward: bool,
        strict: bool,
    ) {
        let Some(search) = self.readline.search.as_ref() else {
            return;
        };
        let found = walk(history, &search.query, start, bound, forward);
        match found {
            Some(found) => self.search_found(history, found),
            None if strict => self.search_failed(),
            None => {}
        }
    }

    /// Show a match, and the prompt without the failure it showed before it.
    fn search_found<S: AsRef<str>>(&mut self, history: &[S], found: Found) {
        let Some(search) = self.readline.search.as_mut() else {
            return;
        };
        search.entry = found.entry;
        search.at = found.at;
        search.failed = false;
        search.prompt();
        self.search_show(history, found);
    }

    /// Say the query found nothing: the match it shows stays where it is.
    fn search_failed(&mut self) {
        let Some(search) = self.readline.search.as_mut() else {
            return;
        };
        search.failed = true;
        search.prompt();
    }

    /// Show a match: the text becomes its entry, with the cursor at the match.
    fn search_show<S: AsRef<str>>(&mut self, history: &[S], found: Found) {
        let text = history[found.entry].as_ref();
        self.gap.clear();
        self.gap.insert(text);
        self.pastes.clear();
        self.gap.move_to(found.at.min(self.gap.len()));
        // Every row of the box wrapped the text that went, so none of them
        // holds a range of this one.
        self.readline.swapped();
        self.drop_rows(0);
    }

    /// Alt+0 to Alt+9: a digit of the argument the next command runs with.
    pub fn digit(&mut self, digit: u8) {
        let readline = &mut self.readline;
        let magnitude = if readline.digits {
            readline
                .arg
                .unsigned_abs()
                .saturating_mul(10)
                .saturating_add(u32::from(digit))
        } else {
            u32::from(digit)
        };
        let value = magnitude.min(ARG_MAX.unsigned_abs()) as i32;
        // A sign with no digit after it takes the first digit: Alt+- Alt+5 is
        // the argument -5, not -15.
        readline.arg = if readline.arg < 0 { -value } else { value };
        readline.digits = true;
    }

    /// Alt+-: a negative argument.
    pub fn negative_argument(&mut self) {
        let readline = &mut self.readline;
        // The sign waits for the digit that follows it; a command that comes
        // first reads an argument of -1.
        readline.arg = if readline.digits {
            -readline.arg.abs()
        } else {
            -1
        };
    }

    /// Drop the argument the digit keys typed: it belongs to the command the
    /// next key runs, so a key that does not read it drops it.
    pub fn clear_argument(&mut self) {
        self.readline.arg = 0;
        self.readline.digits = false;
    }

    /// Insert one typed character: a character typed after another one is the
    /// same undo step.
    pub fn type_char(&mut self, c: char) {
        let typing = self.readline.typing();
        let count = self.readline.take();
        if typing {
            self.readline.last = Last::Type;
        }
        for _ in 0..count.max(0) {
            self.typed_key(c);
        }
    }

    /// Replace the text of `from..to` with `text`, as one undo step.
    pub(super) fn splice(&mut self, from: usize, to: usize, text: &str) {
        let mut removed = String::new();
        if from < to {
            self.readline.room();
            removed = std::mem::take(&mut self.readline.spare);
            removed.clear();
            let (head, tail) = self.gap.slice(from, to);
            removed.push_str(head);
            removed.push_str(tail);
        }
        self.take_step(from, to, text, Removed::Text(removed));
    }

    /// Kill `from..to` into the ring and record the step that takes it back.
    pub(super) fn kill_range(&mut self, from: usize, to: usize, back: bool, join: bool) {
        if from >= to {
            self.readline.last = Last::Kill;
            return;
        }
        let text = self.copied(from, to);
        let chips = self.take_chips(from, to);
        let (entry, range) = self.readline.kill(text, &chips, back, join);
        self.edit_range(from, to, "", Removed::Ring(entry, range), chips);
    }

    /// Replace the text of `from..to` with `text` and record `removed`: the
    /// chips the range covers go whole into the step.
    pub(super) fn take_step(&mut self, from: usize, to: usize, text: &str, removed: Removed) {
        let chips = self.take_chips(from, to);
        self.edit_range(from, to, text, removed, chips);
    }

    /// Replace the text of `from..to` with `text` and record `removed` with the
    /// chips the step keeps.
    fn edit_range(
        &mut self,
        from: usize,
        to: usize,
        text: &str,
        removed: Removed,
        chips: Vec<(usize, Paste)>,
    ) {
        self.gap.erase(from, to);
        self.gap.move_to(from);
        self.gap.insert(text);
        self.moved(from, to - from, 0);
        self.moved(from, 0, text.len());
        self.snap();
        self.touch(from);
        self.readline.push(from, removed, text.len(), chips);
    }

    /// Put the folded chips `chips` into the text at `at`, each with its mark.
    fn put_chips(&mut self, at: usize, chips: &[(usize, Paste)]) {
        for (offset, chip) in chips {
            let mut chip = chip.clone();
            let spot = at + offset;
            chip.put(spot);
            let index = self.pastes.partition_point(|paste| paste.range().0 < spot);
            self.pastes.insert(index, chip);
        }
    }

    /// Take one step of the undo log back: the bytes it added go, what it
    /// removed comes back with the chips it took, and the cursor goes where the
    /// edit started or past the text it put back.
    pub(super) fn undo_step(&mut self) {
        let Some(step) = self.readline.undo.pop() else {
            return;
        };
        let at = step.at;
        let taken = step.removed.len();
        if step.added > 0 {
            let end = at + step.added;
            self.gap.erase(at, end);
            self.moved(at, step.added, 0);
        }
        self.readline.shift(at, step.added, taken);
        let text = step.removed.text();
        if !text.is_empty() {
            self.gap.move_to(at);
            self.gap.insert(text);
            self.moved(at, 0, text.len());
        }
        self.put_chips(at, &step.chips);
        let cursor = step
            .point
            .unwrap_or(if taken > 0 { at + taken } else { at });
        self.gap.move_to(cursor);
        // What the step took back becomes the buffer the next erase writes into,
        // so an undo and the erase after it allocate nothing.
        if let Removed::Text(mut text) = step.removed
            && text.capacity() > self.readline.spare.capacity()
        {
            text.clear();
            std::mem::swap(&mut self.readline.spare, &mut text);
        }
        self.snap();
        self.touch(at);
    }

    /// The argument the digit keys have typed, for the test of its bounds.
    #[cfg(test)]
    pub(super) fn argument(&self) -> i32 {
        self.readline.arg
    }

    /// Insert one typed character as one typed step.
    fn typed_key(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let text = c.encode_utf8(&mut buf);
        let at = self.gap.cursor();
        self.place(text);
        self.readline.typed(at, text.len());
    }
}

/// The match an incremental search walks to: entry `start`, searched from
/// `bound` or from its end, then the entries `forward` walks from it.
fn walk<S: AsRef<str>>(
    history: &[S],
    query: &str,
    start: usize,
    bound: Option<usize>,
    forward: bool,
) -> Option<Found> {
    let line = history.get(start)?.as_ref();
    let first = match (forward, bound) {
        (true, Some(bound)) => find_from(line, query, bound),
        (true, None) => line.find(query),
        (false, Some(bound)) => find_back(line, query, bound),
        (false, None) => line.rfind(query),
    };
    if let Some(at) = first {
        return Some(Found { entry: start, at });
    }
    if forward {
        let newer = history.iter().enumerate().skip(start + 1);
        for (entry, line) in newer {
            if let Some(at) = line.as_ref().find(query) {
                return Some(Found { entry, at });
            }
        }
    } else {
        let older = history.iter().enumerate().take(start).rev();
        for (entry, line) in older {
            if let Some(at) = line.as_ref().rfind(query) {
                return Some(Found { entry, at });
            }
        }
    }
    None
}

/// The last match of `query` in `line` that starts at or before `before`.
fn find_back(line: &str, query: &str, before: usize) -> Option<usize> {
    let mut end = (before + query.len()).min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].rfind(query)
}

/// The first match of `query` in `line` at or after `from`.
fn find_from(line: &str, query: &str, from: usize) -> Option<usize> {
    let mut from = from.min(line.len());
    while !line.is_char_boundary(from) {
        from += 1;
    }
    line[from..].find(query).map(|at| at + from)
}

/// The last match of `query` in `line` that starts before `at`, for a search
/// that steps back from the match it shows.
fn find_before(line: &str, query: &str, at: usize) -> Option<usize> {
    if at == 0 || query.is_empty() {
        return None;
    }
    // A match that starts before `at` ends within the bytes before the one that
    // starts there, so the cut leaves that one out.
    let mut end = at
        .saturating_add(query.len())
        .saturating_sub(1)
        .min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].rfind(query)
}

/// The first match of `query` in `line` that starts after `at`.
fn find_after(line: &str, query: &str, at: usize) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    find_from(line, query, at.saturating_add(1))
}
