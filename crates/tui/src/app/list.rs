//! The list above the text box: what it shows, its query and its pick.
//!
//! One list holds one source: the commands of the `/` list and of the palette,
//! the session folder's files for `@`, or the keys after Ctrl+X. The text after
//! the trigger is the query, and [`crate::pick::Matcher`] ranks the names it
//! matches; the pick is a row of those hits, and the window of at most
//! [`WINDOW`] rows follows the pick.
//!
//! The list holds no drawing: [`crate::app::paint::list`] reads it, and the app
//! that owns it keeps its query and its matcher between keys, so typing in a
//! list allocates nothing once the buffers are warm.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use crate::keyfile::{Foot, Labels};
use crate::keys::{self, Group};
use crate::pick::Matcher;

/// The most item rows the list draws at once.
pub(crate) const WINDOW: usize = 10;

/// The cells of the leader list's key grid in one row.
pub(crate) const PER_ROW: usize = 4;

/// What the list shows.
#[derive(Debug)]
pub(crate) enum Source {
    /// Commands: the `/` list and the palette.
    Commands(Arc<[Command]>),
    /// The session folder's files, relative paths.
    Files(Arc<[Arc<str>]>),
    /// The keys after Ctrl+X, from `keys::hints(Group::Leader)`.
    Leader,
}

/// One command row: its name with the slash, what it does, its key and its group.
#[derive(Debug, Clone)]
pub(crate) struct Command {
    /// The name the text box takes, the slash included.
    pub name: Cow<'static, str>,
    /// The words that say what the command does.
    pub words: Cow<'static, str>,
    /// The keys that run it elsewhere, as the terminal prints them: a `Cow`
    /// because a keys file may have moved one of them.
    pub key: Cow<'static, str>,
    /// The heading the palette lists it under.
    pub group: &'static str,
}

/// The state of the list above the text box.
#[derive(Debug)]
pub(crate) struct List {
    source: Source,
    palette: bool,
    query: String,
    matcher: Matcher,
    selected: usize,
    top: usize,
    /// The leader's own key and its grid's, as a keys file printed them; `None`
    /// for the table's own, which the draw falls back to.
    keys: Option<LeaderKeys>,
    /// The words of the footer row, as a keys file printed them; `None` for the
    /// table's own, which the draw falls back to.
    foot: Option<Arc<str>>,
}

/// The keys the which-key list shows: the leader's own first, then one per hint
/// of `keys::hints(Group::Leader)`, in that order.
#[derive(Debug, Clone)]
pub(crate) struct LeaderKeys {
    pub title: Arc<str>,
    pub grid: Arc<[Arc<str>]>,
}

impl List {
    /// The list of `commands`: the `/` list, or the palette when `palette` is
    /// set, which draws a query row of its own and groups when the query is
    /// empty.
    pub(crate) fn commands(commands: Arc<[Command]>, palette: bool) -> List {
        let mut list = List::empty(Source::Commands(commands), palette);
        list.run();
        list
    }

    /// The list of the session folder's `paths`, in the order the walk found
    /// them.
    pub(crate) fn files(paths: Arc<[Arc<str>]>) -> List {
        let mut list = List::empty(Source::Files(paths), false);
        list.run();
        list
    }

    /// The keys after Ctrl+X, read from the key table.
    pub(crate) fn leader() -> List {
        List::empty(Source::Leader, false)
    }

    fn empty(source: Source, palette: bool) -> List {
        List {
            source,
            palette,
            query: String::new(),
            matcher: Matcher::new(),
            selected: 0,
            top: 0,
            keys: None,
            foot: None,
        }
    }

    /// The words of the footer row, from the labels of a keys file: the keys it
    /// moved are the ones the footer names.
    pub(crate) fn set_labels(&mut self, labels: Option<&Labels>) {
        let Some(labels) = labels else {
            return;
        };
        let foot = match self.source {
            Source::Commands(_) if self.palette => Foot::ListPalette,
            Source::Commands(_) => Foot::ListCommands,
            Source::Files(_) => Foot::ListFiles,
            Source::Leader => Foot::ListLeader,
        };
        self.foot = Some(Arc::from(labels.foot(foot)));
    }

    /// The words of the footer row now, or `None` for the table's.
    pub(crate) fn foot(&self) -> Option<&str> {
        self.foot.as_deref()
    }

    /// The keys the leader's title and grid show, from a keys file.
    pub(crate) fn set_keys(&mut self, keys: LeaderKeys) {
        self.keys = Some(keys);
    }

    /// The keys the leader's title and grid show now, or `None` for the table's.
    pub(crate) fn keys(&self) -> Option<&LeaderKeys> {
        self.keys.as_ref()
    }

    /// Sets the query, the text box text after `/` or `@`, and matches it
    /// again: the pick goes back to the first hit.
    pub(crate) fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        self.run();
    }

    /// Adds one char of the palette's own query.
    pub(crate) fn push(&mut self, c: char) {
        self.query.push(c);
        self.run();
    }

    /// Removes the last char of the palette's own query.
    pub(crate) fn pop(&mut self) {
        if self.query.pop().is_some() {
            self.run();
        }
    }

    /// The query the hits were matched against.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Whether this is the palette, with a query row and groups of its own.
    pub(crate) fn is_palette(&self) -> bool {
        self.palette
    }

    /// Moves the pick `delta` rows and stops at the ends, keeping it inside the
    /// window.
    pub(crate) fn step(&mut self, delta: i8) {
        let Some(last) = self.hits().checked_sub(1) else {
            return;
        };
        let selected = (self.selected as isize + isize::from(delta)).clamp(0, last as isize);
        self.selected = selected as usize;
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + WINDOW {
            self.top = self.selected + 1 - WINDOW;
        }
    }

    /// How many names the query hits.
    pub(crate) fn hits(&self) -> usize {
        self.matcher.hits().len()
    }

    /// The pick, as an index into the hits.
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// The hit indexes the window draws, top down.
    pub(crate) fn window(&self) -> Range<usize> {
        self.top..(self.top + WINDOW).min(self.hits())
    }

    /// The selected command, for Enter and Tab.
    pub(crate) fn command(&self) -> Option<&Command> {
        self.command_at(self.selected)
    }

    /// The command of the hit at `hit`.
    pub(crate) fn command_at(&self, hit: usize) -> Option<&Command> {
        let hit = self.matcher.hits().get(hit)?;
        match &self.source {
            Source::Commands(commands) => commands.get(hit.index as usize),
            Source::Files(_) | Source::Leader => None,
        }
    }

    /// The selected path, for Tab and Enter in the file list.
    pub(crate) fn path(&self) -> Option<&Arc<str>> {
        self.path_at(self.selected)
    }

    /// The path of the hit at `hit`.
    pub(crate) fn path_at(&self, hit: usize) -> Option<&Arc<str>> {
        let hit = self.matcher.hits().get(hit)?;
        match &self.source {
            Source::Files(paths) => paths.get(hit.index as usize),
            Source::Commands(_) | Source::Leader => None,
        }
    }

    /// The char positions the query matched in the hit at `hit`.
    pub(crate) fn marks(&self, hit: usize) -> &[u16] {
        match self.matcher.hits().get(hit) {
            Some(hit) => self.matcher.marks(hit),
            None => &[],
        }
    }

    /// How many names the source holds, whatever the query hits.
    pub(crate) fn total(&self) -> usize {
        match &self.source {
            Source::Commands(commands) => commands.len(),
            Source::Files(paths) => paths.len(),
            Source::Leader => keys::hints(Group::Leader).count(),
        }
    }

    /// What the list shows.
    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// The group rows the palette draws in the window: one for the first row
    /// and one at each change of group, with an empty query only.
    pub(crate) fn group_rows(&self) -> usize {
        if !self.palette || !self.query.is_empty() {
            return 0;
        }
        let mut rows = 0;
        let mut previous = None;
        for hit in self.window() {
            let group = self.command_at(hit).map(|command| command.group);
            if group != previous {
                rows += 1;
                previous = group;
            }
        }
        rows
    }

    /// The rows the list takes: a top row, the item rows, the palette's query
    /// row and group rows, and a footer row.
    pub(crate) fn height(&self) -> u16 {
        let items = self.window().len();
        match &self.source {
            Source::Leader => (3 + self.total().div_ceil(PER_ROW)) as u16,
            Source::Files(_) => (2 + items) as u16,
            Source::Commands(_) => {
                let palette = if self.palette {
                    1 + self.group_rows()
                } else {
                    0
                };
                (2 + items + palette) as u16
            }
        }
    }

    /// The capacities of the query, the hits and the marks, for the test that
    /// checks a second key reuses them.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> (usize, usize, usize) {
        let (hits, marks) = self.matcher.capacity();
        (self.query.capacity(), hits, marks)
    }

    /// Matches the query again and puts the pick on the first hit.
    fn run(&mut self) {
        self.selected = 0;
        self.top = 0;
        // The query leaves the struct for the run and comes back, so the buffer
        // it grew keeps its capacity.
        let query = std::mem::take(&mut self.query);
        match &self.source {
            Source::Commands(commands) => {
                self.matcher
                    .run(&query, commands.iter().map(|command| &*command.name));
            }
            Source::Files(paths) => {
                self.matcher.run(&query, paths.iter().map(|path| &**path));
            }
            Source::Leader => {}
        }
        self.query = query;
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use super::{Command, List};

    /// The four commands the query tests choose between.
    fn commands() -> Arc<[Command]> {
        [
            (
                "/compact",
                "summarize the session to free context",
                "session",
            ),
            ("/model", "pick the model and the provider", "model"),
            ("/mode", "default, plan or yolo", "model"),
            ("/cost", "tokens and cost of this session", "session"),
        ]
        .into_iter()
        .map(|(name, words, group)| Command {
            name: Cow::Borrowed(name),
            words: Cow::Borrowed(words),
            key: Cow::Borrowed(""),
            group,
        })
        .collect::<Vec<_>>()
        .into()
    }

    /// Thirty relative paths, in the order a walk lists them.
    fn paths() -> Arc<[Arc<str>]> {
        (0..30)
            .map(|index| Arc::from(format!("crates/tui/src/file{index:02}.rs")))
            .collect::<Vec<Arc<str>>>()
            .into()
    }

    #[test]
    fn a_slash_query_puts_the_prefix_hit_first() {
        let mut list = List::commands(commands(), false);
        list.set_query("/m");
        assert_eq!(list.command().expect("a hit").name, "/model");
        assert_eq!(list.hits(), 3, "/model, /mode and /compact");
        assert_eq!(list.marks(0), &[] as &[u16], "a prefix hit marks nothing");
        assert_eq!(
            list.marks(2),
            [0, 3],
            "`/compact` matched its `/` and its `m`"
        );
    }

    #[test]
    fn a_step_stops_at_the_ends() {
        let mut list = List::commands(commands(), false);
        list.step(-1);
        assert_eq!(list.selected(), 0, "the first hit is the start");
        for _ in 0..8 {
            list.step(1);
        }
        assert_eq!(list.selected(), 3, "the last hit is the end");
    }

    #[test]
    fn a_new_query_puts_the_pick_back_on_the_first_hit() {
        let mut list = List::commands(commands(), false);
        list.step(1);
        list.step(1);
        assert_eq!(list.selected(), 2);
        list.set_query("/co");
        assert_eq!(list.selected(), 0);
        assert_eq!(list.command().expect("a hit").name, "/compact");
    }

    #[test]
    fn the_window_follows_the_pick_down_thirty_files() {
        let mut list = List::files(paths());
        assert_eq!(list.hits(), 30);
        assert_eq!(list.height(), 12, "a top row, ten items and a footer");
        for _ in 0..15 {
            list.step(1);
        }
        assert_eq!(list.selected(), 15);
        assert_eq!(list.window(), 6..16);
        assert!(list.window().contains(&list.selected()));
        assert_eq!(
            list.path().map(|path| &**path),
            Some("crates/tui/src/file15.rs")
        );
    }

    #[test]
    fn the_palette_types_and_erases_its_query() {
        let mut list = List::commands(commands(), true);
        assert!(list.is_palette());
        assert_eq!(list.hits(), 4);
        list.push('m');
        assert_eq!(list.query(), "m");
        assert_eq!(list.hits(), 3, "/model, /mode and /compact");
        list.pop();
        assert_eq!(list.query(), "");
        assert_eq!(list.hits(), 4);
        list.pop();
        assert_eq!(list.query(), "", "the pop of an empty query keeps it empty");
    }

    #[test]
    fn a_query_that_matches_nothing_gives_no_hit() {
        let mut list = List::commands(commands(), false);
        list.set_query("zzz");
        assert_eq!(list.hits(), 0);
        assert!(list.command().is_none());
        assert!(list.window().is_empty());
        assert_eq!(list.height(), 2, "the top row and the footer stay");
    }

    #[test]
    fn a_second_query_keeps_the_capacity() {
        let mut list = List::commands(commands(), false);
        list.set_query("/ct");
        let first = list.capacity();
        assert_ne!(first.0, 0, "the query keeps its buffer");
        assert_ne!(first.1, 0, "the hits keep theirs");
        assert_ne!(first.2, 0, "the marks keep theirs");
        list.set_query("/cs");
        assert_eq!(list.capacity(), first);
        assert_eq!(list.command().expect("a hit").name, "/cost");
    }

    #[test]
    fn the_palette_counts_a_group_row_per_group_and_a_query_row() {
        let mut list = List::commands(commands(), true);
        assert_eq!(list.group_rows(), 3, "session, model and session again");
        assert_eq!(list.height(), 1 + 1 + 3 + 4 + 1);
        list.push('m');
        assert_eq!(list.group_rows(), 0, "a query drops the groups");
        assert_eq!(list.height(), 1 + 1 + 3 + 1);
    }

    #[test]
    fn the_leader_list_is_its_title_and_the_key_grid() {
        let list = List::leader();
        assert_eq!(list.total(), 6, "the keys after ctrl+x");
        assert_eq!(
            list.height(),
            5,
            "a top row, the title, two rows of keys and a footer"
        );
        assert!(list.command().is_none());
        assert!(list.path().is_none());
    }
}
