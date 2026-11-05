//! The Changes panel: the files this session changed, with their line counts.
//!
//! The panel adds the `write` and `edit` calls the host hands it and reads no
//! journal itself. The arguments decode with borrowed serde into `Cow<str>`, so
//! counting the lines of a write's content copies none of it, and a row's text
//! is built when its counts change, so a draw allocates nothing.

use std::borrow::Cow;
use std::sync::Arc;

use serde::Deserialize;

use crate::slot::{Component, Panel, Priority, Size, Text, Update};

/// The fold order: the change list folds after the context use.
const PRIORITY: Priority = 5;

/// The narrowest sidebar this panel draws in.
const MIN_WIDTH: u16 = 20;

/// The columns a row's path keeps: a 28-column sidebar leaves the path this
/// room beside its `+A -R` note.
const PATH_COLUMNS: usize = 18;

/// What a path cut short starts with.
const ELLIPSIS: char = '…';

/// The arguments of a `write` call.
#[derive(Debug, Deserialize)]
struct WriteArgs<'a> {
    #[serde(borrow)]
    path: Cow<'a, str>,
    #[serde(borrow)]
    content: Cow<'a, str>,
}

/// The arguments of an `edit` call: one replacement, or a list of them.
#[derive(Debug, Deserialize)]
struct EditArgs<'a> {
    #[serde(borrow)]
    path: Cow<'a, str>,
    #[serde(default, borrow)]
    old_string: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    new_string: Option<Cow<'a, str>>,
    #[serde(default)]
    edits: Option<Vec<Replacement<'a>>>,
}

/// One replacement of an `edit` call's list.
#[derive(Debug, Deserialize)]
struct Replacement<'a> {
    #[serde(borrow)]
    old_string: Cow<'a, str>,
    #[serde(borrow)]
    new_string: Cow<'a, str>,
}

/// One file as the panel draws it.
#[derive(Debug)]
struct Line {
    /// The path as the tool named it, which finds the row again.
    path: Arc<str>,
    /// The path as the panel shows it, short enough for a narrow sidebar.
    text: Arc<str>,
    /// `+3 -1`.
    note: Arc<str>,
    /// The byte of `note` where the removed count starts.
    note_split: u32,
    /// The lines added so far.
    added: u64,
    /// The lines removed so far.
    removed: u64,
}

/// The Changes panel.
#[derive(Debug)]
pub struct ChangesPanel {
    title: Text,
    lines: Vec<Line>,
    /// How many rows, and the lines they add and remove, as the header row
    /// reads them: `3 files +91 -15`.
    tally: (usize, u64, u64),
    /// The number of rows the header row reads.
    files: Text,
    /// The lines added so far, as `+91`.
    added: Text,
    /// The lines removed so far, as `-15`.
    removed: Text,
}

impl ChangesPanel {
    /// An empty panel.
    pub fn new() -> ChangesPanel {
        ChangesPanel {
            title: Text::Static("Changes"),
            lines: Vec::new(),
            tally: (0, 0, 0),
            files: Text::Static("0"),
            added: Text::Static("+0"),
            removed: Text::Static("-0"),
        }
    }

    /// The number of files that changed, as the header row reads it.
    pub fn files(&self) -> &Text {
        &self.files
    }

    /// The lines added so far, as the header row reads them: `+91`.
    pub fn added(&self) -> &Text {
        &self.added
    }

    /// The lines removed so far, as the header row reads them: `-15`.
    pub fn removed(&self) -> &Text {
        &self.removed
    }

    /// Build the tally the header row shows, which changes only with a row or a
    /// count.
    fn build_tally(&mut self) {
        let files = self.lines.len();
        let added: u64 = self.lines.iter().map(|line| line.added).sum();
        let removed: u64 = self.lines.iter().map(|line| line.removed).sum();
        if self.tally == (files, added, removed) {
            return;
        }
        self.tally = (files, added, removed);
        self.files = Text::Shared(Arc::from(files.to_string()));
        self.added = Text::Shared(Arc::from(format!("+{added}")));
        self.removed = Text::Shared(Arc::from(format!("-{removed}")));
    }

    /// Take a `write` call: its content's lines are the lines it adds, and a
    /// second write of the same path replaces the counts of the first.
    fn write(&mut self, arguments: &str) -> bool {
        let Ok(args) = serde_json::from_str::<WriteArgs<'_>>(arguments) else {
            return false;
        };
        let added = line_count(&args.content);
        let (note, split) = note_of(added, 0);
        let line = self.line(&args.path);
        line.added = added;
        line.removed = 0;
        line.note = note;
        line.note_split = split;
        self.build_tally();
        true
    }

    /// Take an `edit` call: every replacement removes the lines of its old
    /// text and adds the lines of its new one.
    fn edit(&mut self, arguments: &str) -> bool {
        let Ok(args) = serde_json::from_str::<EditArgs<'_>>(arguments) else {
            return false;
        };
        let mut added = 0;
        let mut removed = 0;
        let mut replaces = false;
        if let (Some(old), Some(new)) = (&args.old_string, &args.new_string) {
            added += line_count(new);
            removed += line_count(old);
            replaces = true;
        }
        if let Some(edits) = &args.edits {
            for edit in edits {
                added += line_count(&edit.new_string);
                removed += line_count(&edit.old_string);
                replaces = true;
            }
        }
        if !replaces {
            // An edit that names no replacement changes no file.
            return false;
        }
        let line = self.line(&args.path);
        line.added += added;
        line.removed += removed;
        let (note, split) = note_of(line.added, line.removed);
        line.note = note;
        line.note_split = split;
        self.build_tally();
        true
    }

    /// The row of `path`, pushed at the end in first-seen order when new.
    fn line(&mut self, path: &str) -> &mut Line {
        let at = self.lines.iter().position(|line| &*line.path == path);
        let at = at.unwrap_or_else(|| {
            self.lines.push(Line {
                path: Arc::from(path),
                text: Arc::from(shorten(path)),
                note: Arc::from("+0"),
                note_split: 2,
                added: 0,
                removed: 0,
            });
            self.lines.len() - 1
        });
        &mut self.lines[at]
    }
}

impl Default for ChangesPanel {
    fn default() -> Self {
        ChangesPanel::new()
    }
}

impl Panel for ChangesPanel {
    fn id(&self) -> &str {
        "changes"
    }

    fn title(&self) -> &Text {
        &self.title
    }

    fn priority(&self) -> Priority {
        PRIORITY
    }

    fn min_width(&self) -> u16 {
        MIN_WIDTH
    }

    fn update(&mut self, update: &Update<'_>) -> bool {
        let Update::ToolCall { name, arguments } = *update else {
            return false;
        };
        match name {
            "write" => self.write(arguments),
            "edit" => self.edit(arguments),
            _ => false,
        }
    }

    fn draw(&self, _size: Size, out: &mut Vec<Component>) {
        for line in &self.lines {
            out.push(Component::item_scored(
                &line.text,
                &line.note,
                line.note_split as usize,
                false,
            ));
        }
        // One blank row between the panels, under a list that has rows.
        if !self.lines.is_empty() {
            out.push(Component::text(Text::Static("")));
        }
    }
}

/// How many lines a text has: the lines a write of it adds, and the lines a
/// replacement of it takes away. A last newline ends a line, it does not start
/// one.
fn line_count(text: &str) -> u64 {
    text.lines().count() as u64
}

/// The note of a row and the byte where its removed count starts: `+3 -1`,
/// with `-0` left out. The bytes before the split draw in the add role and the
/// rest in the delete role.
fn note_of(added: u64, removed: u64) -> (Arc<str>, u32) {
    let plus = format!("+{added}");
    if removed == 0 {
        let split = plus.len() as u32;
        (Arc::from(plus), split)
    } else {
        let split = (plus.len() + 1) as u32;
        (Arc::from(format!("{plus} -{removed}")), split)
    }
}

/// `path` cut to [`PATH_COLUMNS`], starting at a directory boundary where one
/// is in reach and always keeping the file name whole.
fn shorten(path: &str) -> String {
    let columns = path.chars().count();
    if columns <= PATH_COLUMNS {
        return path.to_owned();
    }
    // The file name always stays whole, and the ellipsis takes one column.
    let name = path.rfind('/').map_or(0, |at| at + 1);
    let room = PATH_COLUMNS - 1;
    let mut out = String::with_capacity(PATH_COLUMNS * 4);
    out.push(ELLIPSIS);
    if path[name..].chars().count() >= room {
        // A file name too long for the row shows its last columns.
        out.extend(path.chars().skip(columns - room));
        return out;
    }
    let tail = path
        .char_indices()
        .nth(columns - room)
        .map_or(name, |(at, _)| at);
    let start = path[tail..].find('/').map_or(tail, |at| tail + at + 1);
    out.push_str(&path[start.min(name)..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::Role;

    fn call(panel: &mut ChangesPanel, name: &str, arguments: &str) -> bool {
        panel.update(&Update::ToolCall { name, arguments })
    }

    fn drawn(panel: &ChangesPanel) -> Vec<Component> {
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        out
    }

    /// Every component as text, for comparing two draws.
    fn rendered(components: &[Component]) -> Vec<String> {
        components
            .iter()
            .map(|component| format!("{component:?}"))
            .collect()
    }

    /// The text of a list row.
    fn text_of(component: &Component) -> &str {
        match component {
            Component::Item { text, .. } => text.as_str(),
            other => panic!("{other:?} is not a row"),
        }
    }

    /// The note of a list row.
    fn note_of(component: &Component) -> &str {
        match component {
            Component::Item { note, .. } => note.as_str(),
            other => panic!("{other:?} is not a row"),
        }
    }

    #[test]
    fn a_write_and_an_edit_show_their_line_counts() {
        let mut panel = ChangesPanel::new();
        let ten = "one\\ntwo\\nthree\\nfour\\nfive\\nsix\\nseven\\neight\\nnine\\nten\\n";
        assert!(call(
            &mut panel,
            "write",
            &format!(r#"{{"path":"a.rs","content":"{ten}"}}"#)
        ));
        assert!(call(
            &mut panel,
            "edit",
            r#"{"path":"b.rs","old_string":"old","new_string":"new\nlines"}"#
        ));
        let out = drawn(&panel);
        assert_eq!(out.len(), 3, "two rows and the blank one: {out:?}");
        assert_eq!(text_of(&out[0]), "a.rs");
        assert_eq!(note_of(&out[0]), "+10");
        assert_eq!(text_of(&out[1]), "b.rs");
        assert_eq!(note_of(&out[1]), "+2 -1");
        assert_eq!(panel.files().as_str(), "2");
        assert_eq!(panel.added().as_str(), "+12");
        assert_eq!(panel.removed().as_str(), "-1");
    }

    #[test]
    fn the_tally_draws_added_in_the_add_role_and_removed_in_the_delete_role() {
        let mut panel = ChangesPanel::new();
        assert!(call(
            &mut panel,
            "edit",
            r#"{"path":"a.rs","old_string":"old","new_string":"one\ntwo"}"#
        ));
        let out = drawn(&panel);
        let Component::Item { note, .. } = &out[0] else {
            panic!("{:?} is not a row", out[0]);
        };
        assert_eq!(note.as_str(), "+2 -1");
        let split = note.split() as usize;
        assert_eq!(&note.as_str()[..split], "+2 ");
        assert_eq!(&note.as_str()[split..], "-1");
        assert_eq!(note.first(), Role::Good, "the added count");
        assert_eq!(note.second(), Role::Bad, "the removed count");
    }

    #[test]
    fn an_edits_list_adds_up() {
        let mut panel = ChangesPanel::new();
        assert!(call(
            &mut panel,
            "edit",
            r#"{"path":"a.rs","edits":[
                {"old_string":"one","new_string":"1\ntwo"},
                {"old_string":"a\nb","new_string":"c"}
            ]}"#
        ));
        let out = drawn(&panel);
        assert_eq!(out.len(), 2, "the row and the blank one: {out:?}");
        assert_eq!(note_of(&out[0]), "+3 -3");
    }

    #[test]
    fn a_second_write_of_a_path_replaces_its_counts() {
        let mut panel = ChangesPanel::new();
        assert!(call(
            &mut panel,
            "write",
            r#"{"path":"a.rs","content":"one\ntwo"}"#
        ));
        assert!(call(
            &mut panel,
            "write",
            r#"{"path":"a.rs","content":"one"}"#
        ));
        let out = drawn(&panel);
        assert_eq!(out.len(), 2, "one row per path, and the blank one");
        assert_eq!(note_of(&out[0]), "+1");
        assert_eq!(panel.files().as_str(), "1");
    }

    #[test]
    fn bad_json_and_another_tool_change_nothing() {
        let mut panel = ChangesPanel::new();
        assert!(call(
            &mut panel,
            "write",
            r#"{"path":"a.rs","content":"one"}"#
        ));
        let before = rendered(&drawn(&panel));
        for update in [
            Update::ToolCall {
                name: "write",
                arguments: "{not json",
            },
            Update::ToolCall {
                name: "edit",
                arguments: r#"{"path":"b.rs","old_string":"x"}"#,
            },
            Update::ToolCall {
                name: "read",
                arguments: r#"{"path":"c.rs"}"#,
            },
            Update::Tick,
        ] {
            assert!(!panel.update(&update), "{update:?}");
        }
        assert_eq!(rendered(&drawn(&panel)), before);
    }

    #[test]
    fn a_long_path_keeps_its_file_name() {
        let mut panel = ChangesPanel::new();
        assert!(call(
            &mut panel,
            "write",
            r#"{"path":"crates/a/deeply/nested/folder/src/path.rs","content":"one"}"#
        ));
        let out = drawn(&panel);
        let text = text_of(&out[0]);
        assert!(text.chars().count() <= PATH_COLUMNS, "{text:?} is too wide");
        assert!(text.ends_with("path.rs"), "{text:?}");
        assert!(text.starts_with(ELLIPSIS), "{text:?}");
    }

    #[test]
    fn a_second_draw_keeps_its_buffer_and_returns_equal_components() {
        let mut panel = ChangesPanel::new();
        call(
            &mut panel,
            "write",
            r#"{"path":"a.rs","content":"one\ntwo\nthree"}"#,
        );
        call(
            &mut panel,
            "edit",
            r#"{"path":"b.rs","old_string":"one","new_string":"two\nthree"}"#,
        );
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        let capacity = out.capacity();
        let first = rendered(&out);
        for width in [28, 40, 28] {
            out.clear();
            panel.draw(Size::new(width, 12), &mut out);
            assert_eq!(out.capacity(), capacity, "the buffer grew");
            assert_eq!(rendered(&out), first, "the same components drew");
        }
    }

    #[test]
    fn a_session_that_changed_nothing_draws_no_row() {
        let panel = ChangesPanel::new();
        assert!(drawn(&panel).is_empty());
    }
}
