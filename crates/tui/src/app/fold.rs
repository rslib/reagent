//! The fold state of the transcript's blocks.
//!
//! A block is folded until the user opens it: a tool call shows its one-line
//! row, the newest edit shows the first [`DIFF_FOLD_LINES`] lines of its diff,
//! a thinking block its one dim row. Tab folds or opens the block under the
//! cursor, or the newest block when none is under it; Ctrl+O opens every block.
//!
//! The state is a fixed array of block numbers, so a key allocates nothing and a
//! frame that asks whether a block is open scans at most [`OPEN_MAX`] numbers.

/// Most blocks the fold state remembers open at once. Past this the oldest open
/// block folds again, so a run of any length keeps a fixed state.
pub const OPEN_MAX: usize = 64;

/// The diff lines a folded edit shows before the rest is folded away.
pub const DIFF_FOLD_LINES: usize = 4;

/// The blocks of the transcript the user opened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Folds {
    /// The numbers of the open blocks, oldest first.
    open: [u64; OPEN_MAX],
    /// How many of `open` are used.
    len: usize,
    /// Every block is open, until one of them is folded.
    all: bool,
    /// The newest block number; the transcript numbers its blocks from 1.
    newest: u64,
    /// The newest call that made a diff: the block that holds it shows those
    /// lines, and a block with an older edit shows its row alone.
    diff: u64,
}

impl Default for Folds {
    fn default() -> Self {
        Folds::new()
    }
}

impl Folds {
    /// No block of a transcript that holds none.
    pub(crate) const fn new() -> Self {
        Folds {
            open: [0; OPEN_MAX],
            len: 0,
            all: false,
            newest: 0,
            diff: 0,
        }
    }

    /// Stores a block the transcript just added and answers its number. A new
    /// block is folded, and open once Ctrl+O opened every block.
    pub(crate) fn add(&mut self) -> u64 {
        self.newest = self.newest.saturating_add(1);
        self.newest
    }

    /// The number of the newest block, 0 while the transcript holds none.
    #[allow(dead_code, reason = "the sidebar panel draws it once it lists blocks")]
    pub(crate) fn newest(&self) -> u64 {
        self.newest
    }

    /// Whether every block is open, as Ctrl+O left them.
    pub(crate) fn all(&self) -> bool {
        self.all
    }

    /// Whether the block `id` is open.
    pub(crate) fn is_open(&self, id: u64) -> bool {
        self.all || self.open[..self.len].contains(&id)
    }

    /// Opens the block `id`, or folds it when it was open.
    pub(crate) fn toggle(&mut self, id: u64) {
        if self.all {
            // One block folds, the rest stay as Ctrl+O left them.
            self.all = false;
            self.fill_open();
            self.remove(id);
            return;
        }
        if !self.remove(id) {
            self.push(id);
        }
    }

    /// Opens every block, the newest [`OPEN_MAX`] of them kept open after one of
    /// them folds.
    pub(crate) fn open_all(&mut self) {
        self.all = true;
    }

    /// Folds every block.
    #[allow(
        dead_code,
        reason = "the key that folds every block lands with the key map"
    )]
    pub(crate) fn fold_all(&mut self) {
        self.all = false;
        self.len = 0;
    }

    /// The block Tab acts on: the one under the cursor, else the newest.
    pub(crate) fn target(&self, cursor: Option<u64>) -> Option<u64> {
        cursor.or((self.newest > 0).then_some(self.newest))
    }

    /// Opens or folds the block Tab acts on.
    pub(crate) fn toggle_at(&mut self, cursor: Option<u64>) {
        if let Some(id) = self.target(cursor) {
            self.toggle(id);
        }
    }

    /// The newest call that made a diff, 0 while no call made one.
    pub(crate) const fn diff(&self) -> u64 {
        self.diff
    }

    /// Marks the call `id` as the one that shows its diff, and answers the call
    /// that showed one before it.
    pub(crate) fn show_diff(&mut self, id: u64) -> u64 {
        std::mem::replace(&mut self.diff, id)
    }

    /// The diff lines the block of the call `id`, which holds `lines` of them,
    /// shows while it is `open` or folded: an open block shows every line, the
    /// block of the newest edit the first few, and one of an older edit none.
    pub(crate) const fn diff_shown(&self, id: u64, lines: usize, open: bool) -> usize {
        if open {
            return lines;
        }
        if self.diff != 0 && self.diff != id {
            return 0;
        }
        if lines < DIFF_FOLD_LINES {
            lines
        } else {
            DIFF_FOLD_LINES
        }
    }

    /// Turns "every block is open" into the numbers of the newest blocks, so one
    /// of them can fold without folding the rest.
    fn fill_open(&mut self) {
        self.all = false;
        self.len = 0;
        let mut id = self.newest;
        while id > 0 && self.len < OPEN_MAX {
            self.push(id);
            id -= 1;
        }
    }

    /// Adds `id` to the open blocks, dropping the oldest when the array is full.
    fn push(&mut self, id: u64) {
        if self.len == OPEN_MAX {
            self.open.copy_within(1.., 0);
            self.len -= 1;
        }
        self.open[self.len] = id;
        self.len += 1;
    }

    /// Takes `id` out of the open blocks, and answers whether it was in them.
    fn remove(&mut self, id: u64) -> bool {
        let Some(at) = self.open[..self.len].iter().position(|open| *open == id) else {
            return false;
        };
        self.open.copy_within(at + 1..self.len, at);
        self.len -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transcript of `blocks` blocks, all folded.
    fn folds(blocks: u64) -> Folds {
        let mut folds = Folds::new();
        for _ in 0..blocks {
            folds.add();
        }
        folds
    }

    #[test]
    fn fold_numbers_new_blocks_and_keeps_them_folded() {
        let mut folds = Folds::new();
        assert_eq!(folds.newest(), 0);
        assert_eq!(folds.add(), 1);
        assert_eq!(folds.add(), 2);
        assert!(!folds.is_open(1), "a new block is folded");
        assert!(!folds.is_open(2), "a new block is folded");
    }

    #[test]
    fn fold_opens_and_folds_the_block_it_names() {
        let mut folds = folds(3);
        folds.toggle(2);
        assert!(folds.is_open(2));
        assert!(!folds.is_open(1) && !folds.is_open(3));
        folds.toggle(2);
        assert!(!folds.is_open(2), "the second tab folds it again");
    }

    #[test]
    fn fold_target_is_the_cursor_and_else_the_newest_block() {
        let mut folds = folds(3);
        assert_eq!(folds.target(None), Some(3));
        assert_eq!(folds.target(Some(1)), Some(1));
        folds.toggle_at(None);
        assert!(
            folds.is_open(3),
            "tab with no cursor opens the newest block"
        );
        assert_eq!(Folds::new().target(None), None, "an empty transcript");
    }

    #[test]
    fn fold_open_all_opens_every_block_and_the_next_tab_folds_only_one() {
        let mut folds = folds(4);
        folds.open_all();
        assert!((1..=4).all(|id| folds.is_open(id)));
        folds.toggle(2);
        assert!(!folds.is_open(2), "tab folds the block it names");
        assert!(
            folds.is_open(1) && folds.is_open(3) && folds.is_open(4),
            "the other blocks stay open"
        );
        folds.fold_all();
        assert!((1..=4).all(|id| !folds.is_open(id)));
    }

    #[test]
    fn fold_keeps_no_more_than_the_newest_open_blocks() {
        let mut folds = folds(OPEN_MAX as u64 + 10);
        folds.open_all();
        folds.toggle(1);
        let newest = folds.newest();
        assert!(folds.is_open(newest), "the newest block stays open");
        let open = (1..=newest).filter(|id| folds.is_open(*id)).count();
        assert!(open <= OPEN_MAX, "{open} blocks are open");
    }

    #[test]
    fn fold_a_diff_shows_four_lines_until_it_opens() {
        let folds = folds(0);
        assert_eq!(folds.diff_shown(1, 9, false), DIFF_FOLD_LINES);
        assert_eq!(folds.diff_shown(1, 9, true), 9);
        assert_eq!(
            folds.diff_shown(1, 2, false),
            2,
            "a short diff folds nothing"
        );
    }

    #[test]
    fn fold_only_the_newest_diff_shows_its_lines() {
        let mut folds = folds(0);
        assert_eq!(folds.diff(), 0);
        assert_eq!(folds.show_diff(2), 0, "no block showed one before it");
        assert_eq!(folds.diff_shown(2, 9, false), DIFF_FOLD_LINES);
        assert_eq!(folds.diff_shown(1, 9, false), 0, "an older edit shows none");
        assert_eq!(folds.diff_shown(1, 9, true), 9, "the tab opens all of it");
        assert_eq!(folds.show_diff(3), 2, "the block that showed one before it");
        assert_eq!(folds.diff_shown(2, 9, false), 0);
    }
}
