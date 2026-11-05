//! The key help sheet.
//!
//! The rows are the one key map, built once when the sheet opens: a group row
//! for where each set of keys works, then one item per hint with the keys as
//! its name and the words as its detail. A filter only reorders row indices, so
//! it allocates nothing per key.

use std::sync::Arc;

use crate::keys::{Group, HINTS, hints};

use super::{Kind, Mark, Row, Sheet};

/// The groups in the order the sheet lists them, with the name each heads its
/// lane with.
const GROUPS: [(Group, &str); 3] = [
    (Group::TextBox, "text box"),
    (Group::Transcript, "transcript"),
    (Group::Leader, "ctrl+x then"),
];

/// The key sheet: every key of the one key map, grouped, with the groups side
/// by side when there is room.
pub(crate) fn help_sheet() -> Sheet {
    let none: Arc<str> = Arc::from("");
    let mut rows = Vec::with_capacity(HINTS.len() + GROUPS.len());
    for (group, name) in GROUPS {
        rows.push(Row {
            kind: Kind::Group,
            mark: Mark::None,
            name: Arc::from(name),
            detail: none.clone(),
            right: none.clone(),
            dim: false,
        });
        for hint in hints(group) {
            rows.push(Row {
                kind: Kind::Item,
                mark: Mark::None,
                name: Arc::from(hint.keys),
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
        "esc closes \u{b7} ctrl+x / runs any command by name",
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
        super::help_sheet()
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
                "ctrl+r ctrl+s",
                "ctrl+p ctrl+n",
                "ctrl+o",
                "alt+0-9 ctrl+space",
            ]
        );
    }
}
