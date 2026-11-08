//! The key help sheet.
//!
//! The rows are the one key map, built once when the sheet opens: a group row
//! for where each set of keys works, then one item per hint with the keys as
//! its name and the words as its detail. A filter only reorders row indices, so
//! it allocates nothing per key.

use std::sync::Arc;

use crate::keyfile::Labels;
use crate::keys::{Group, HINTS};

use super::{Kind, Mark, Row, Sheet};

/// The keys of the footer row, until a keys file moves one.
const FOOTER: &str = "esc closes \u{b7} ctrl+x / runs any command by name";

/// The groups in the order the sheet lists them, with the name each heads its
/// lane with.
const GROUPS: [(Group, &str); 3] = [
    (Group::TextBox, "text box"),
    (Group::Transcript, "transcript"),
    (Group::Leader, "ctrl+x then"),
];

/// The key sheet: every key of the one key map, grouped, with the groups side
/// by side when there is room.
pub(crate) fn help_sheet(labels: Option<&Labels>) -> Sheet {
    let none: Arc<str> = Arc::from("");
    let mut rows = Vec::with_capacity(HINTS.len() + GROUPS.len());
    for (group, name) in GROUPS {
        // The leader's heading names the key it waits for, which a file moves.
        let name = match (group, labels) {
            (Group::Leader, Some(labels)) => Arc::from(format!("{} then", labels.leader())),
            _ => Arc::from(name),
        };
        rows.push(Row {
            kind: Kind::Group,
            mark: Mark::None,
            name,
            detail: none.clone(),
            right: none.clone(),
            dim: false,
        });
        for (index, hint) in HINTS
            .iter()
            .enumerate()
            .filter(|(_, hint)| hint.group == group)
        {
            rows.push(Row {
                kind: Kind::Item,
                mark: Mark::None,
                name: Arc::from(match labels {
                    Some(labels) => labels.hint(index),
                    None => hint.keys,
                }),
                detail: Arc::from(hint.words),
                right: none.clone(),
                dim: false,
            });
        }
    }
    Sheet::new(
        "Keys",
        Arc::from(format!("{} keys", HINTS.len())),
        "type to filter",
        FOOTER,
        rows,
    )
    .with_columns()
}

#[cfg(test)]
mod tests {
    use crate::app::sheet::{Kind, Sheet};
    use crate::keys::{Group, HINTS, hints};

    /// The sheet of the tests.
    fn sheet() -> Sheet {
        super::help_sheet(None)
    }

    /// The names of the rows on screen, top down.
    fn names(sheet: &Sheet) -> Vec<&str> {
        sheet
            .shown
            .iter()
            .map(|&index| &*sheet.rows[index as usize].name)
            .collect()
    }

    /// The sheet lists the three groups in order, and each group holds every
    /// hint of the one key map, keys and words alike.
    #[test]
    fn the_three_groups_hold_every_hint_of_the_key_map() {
        let sheet = sheet();
        let mut groups: Vec<&str> = Vec::new();
        let mut items: Vec<Vec<(&str, &str)>> = Vec::new();
        for row in &sheet.rows {
            match row.kind {
                Kind::Group => {
                    groups.push(&row.name);
                    items.push(Vec::new());
                }
                Kind::Item => items
                    .last_mut()
                    .expect("an item under a group")
                    .push((&row.name, &row.detail)),
                _ => {}
            }
        }
        let wanted = [
            ("text box", Group::TextBox),
            ("transcript", Group::Transcript),
            ("ctrl+x then", Group::Leader),
        ];
        assert_eq!(groups, wanted.map(|(name, _)| name));
        for (at, (_, group)) in wanted.into_iter().enumerate() {
            let rows: Vec<(&str, &str)> =
                hints(group).map(|hint| (hint.keys, hint.words)).collect();
            assert_eq!(items[at], rows);
        }
    }

    /// The count is the number of keys the sheet lists.
    #[test]
    fn the_count_is_the_number_of_hints() {
        let sheet = sheet();
        assert_eq!(&*sheet.count, format!("{} keys", HINTS.len()));
        assert_eq!(sheet.title, "Keys");
        assert_eq!(sheet.hint, "type to filter");
        assert_eq!(
            sheet.footer,
            "esc closes \u{b7} ctrl+x / runs any command by name"
        );
    }

    /// A keys file moves the keys the sheet lists, and the sheet names them.
    #[test]
    fn custom_key_label_help_sheet_row() {
        let (file, notes) = crate::keyfile::KeyFile::parse(
            "[keys]\nsend = \"ctrl+s\"\nsearch-forward = \"alt+s\"\n",
        );
        assert!(notes.is_empty(), "{notes:?}");
        let sheet = super::help_sheet(Some(file.labels()));
        let rows: Vec<(&str, &str)> = sheet
            .rows
            .iter()
            .filter(|row| row.kind == Kind::Item)
            .map(|row| (&*row.name, &*row.detail))
            .collect();
        assert_eq!(
            rows[0],
            ("ctrl+s", "send or steer"),
            "the send row shows the key it moved to"
        );
        assert!(
            rows.iter().all(|(keys, _)| *keys != "enter"),
            "no row shows the key send left: {rows:?}"
        );
        assert_eq!(rows[18], ("ctrl+r alt+s", "search history"));
    }

    /// The groups flow side by side when the width leaves room for their lanes.
    #[test]
    fn the_groups_flow_side_by_side() {
        assert!(sheet().columns);
    }

    /// A filter on the keys keeps the keys it matches and leaves the group rows
    /// out.
    #[test]
    fn a_filter_on_the_keys_finds_the_ctrl_keys() {
        let mut sheet = sheet();
        for c in "ctrl".chars() {
            sheet.push(c);
        }
        assert_eq!(
            names(&sheet),
            [
                "ctrl+x /",
                "ctrl+c ctrl+c",
                "ctrl+a ctrl+e",
                "ctrl+k ctrl+u ctrl+w",
                "ctrl+y alt+y",
                "ctrl+t alt+t",
                "ctrl+_",
                "ctrl+q",
                "ctrl+v",
                "ctrl+r ctrl+s",
                "ctrl+p ctrl+n",
                "ctrl+o",
                "esc ctrl+g",
                "alt+0-9 ctrl+space",
            ]
        );
    }

    /// A file that moves nothing prints each word as the table has it.
    #[test]
    fn keys_in_force_help_foot_matches_the_table() {
        let (file, _) = crate::keyfile::KeyFile::parse("");
        let labels = file.labels();
        assert_eq!(labels.foot(crate::keyfile::Foot::Help), super::FOOTER);
    }
}
