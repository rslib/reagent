//! The command table: the names the `/` list shows and the palette filters.
//!
//! One table is built per app and shared by every list it opens, so the names,
//! the words and the keys a row draws are the ones the help sheet names. The
//! keys are built again when a keys file is read, from the words it printed.

use std::borrow::Cow;
use std::sync::Arc;

use crate::keyfile::Labels;

use super::list::Command;

/// The group the palette lists the plugin commands under, and the mark the `/`
/// list uses to tell a plugin row from a built-in one.
pub(crate) const PLUGIN_GROUP: &str = "plugin";

/// The group the palette lists the skills of the session under, and the mark the
/// `/` list uses to tell a skill row from a built-in one.
pub(crate) const SKILL_GROUP: &str = "skill";

/// The commands, in the order an empty list shows them, with the key the table
/// gives each one until a keys file moves it.
const TABLE: [Command; 12] = [
    Command {
        name: Cow::Borrowed("/model"),
        words: Cow::Borrowed("pick the model and the provider"),
        key: Cow::Borrowed("ctrl+x m"),
        group: "model",
    },
    Command {
        name: Cow::Borrowed("/level"),
        words: Cow::Borrowed("reasoning effort"),
        key: Cow::Borrowed("ctrl+x t"),
        group: "model",
    },
    Command {
        name: Cow::Borrowed("/mode"),
        words: Cow::Borrowed("permission mode"),
        key: Cow::Borrowed(""),
        group: "model",
    },
    Command {
        name: Cow::Borrowed("/sessions"),
        words: Cow::Borrowed("open another session"),
        key: Cow::Borrowed("ctrl+x l"),
        group: "session",
    },
    Command {
        name: Cow::Borrowed("/new"),
        words: Cow::Borrowed("start a new session"),
        key: Cow::Borrowed(""),
        group: "session",
    },
    Command {
        name: Cow::Borrowed("/fork"),
        words: Cow::Borrowed("fork this session into a new one"),
        key: Cow::Borrowed(""),
        group: "session",
    },
    Command {
        name: Cow::Borrowed("/compact"),
        words: Cow::Borrowed("compact the history into a checkpoint"),
        key: Cow::Borrowed(""),
        group: "session",
    },
    Command {
        name: Cow::Borrowed("/login"),
        words: Cow::Borrowed("log in to a provider"),
        key: Cow::Borrowed(""),
        group: "session",
    },
    Command {
        name: Cow::Borrowed("/cost"),
        words: Cow::Borrowed("tokens and cost of this session"),
        key: Cow::Borrowed(""),
        group: "view",
    },
    Command {
        name: Cow::Borrowed("/help"),
        words: Cow::Borrowed("every key"),
        key: Cow::Borrowed("?"),
        group: "view",
    },
    Command {
        name: Cow::Borrowed("/quit"),
        words: Cow::Borrowed("leave the session"),
        key: Cow::Borrowed("ctrl+c ctrl+c"),
        group: "app",
    },
    Command {
        name: Cow::Borrowed("/skills"),
        words: Cow::Borrowed("the skills of this session"),
        key: Cow::Borrowed(""),
        group: "skills",
    },
];

/// Whether a built-in row of the table holds `name`, and so keeps its meaning
/// when a plugin or a skill offers the same one.
pub(crate) fn builtin(name: &str) -> bool {
    TABLE.iter().any(|row| &*row.name == name)
}

/// The commands the app's lists open with, built once and shared: the keys of
/// `labels` when a keys file was read, and the table's own when none was.
pub(crate) fn table(labels: Option<&Labels>) -> Arc<[Command]> {
    let Some(labels) = labels else {
        return Arc::from(TABLE.to_vec());
    };
    Arc::from(
        TABLE
            .iter()
            .enumerate()
            .map(|(index, row)| Command {
                name: row.name.clone(),
                words: row.words.clone(),
                key: Cow::Owned(labels.command(index).to_owned()),
                group: row.group,
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::{TABLE, table};
    use crate::keyfile::KeyFile;

    /// A keys file that moves none of the commands' keys leaves every row with
    /// the key the table names: the words of the file are in the table's order.
    #[test]
    fn the_labels_keep_the_keys_of_the_table() {
        let (file, notes) = KeyFile::parse("");
        assert!(notes.is_empty(), "{notes:?}");
        let rows = table(Some(file.labels()));
        assert_eq!(rows.len(), TABLE.len(), "every row and no other");
        for (row, labelled) in TABLE.iter().zip(rows.iter()) {
            assert_eq!(&*labelled.key, &*row.key, "the key of {}", row.name);
        }
    }

    /// A moved leader is the first key of the rows that name it, and the rows
    /// with no key keep none.
    #[test]
    fn a_moved_leader_reaches_the_rows_that_name_it() {
        let (file, notes) = KeyFile::parse("[keys]\nleader = \"alt+x\"\n");
        assert!(notes.is_empty(), "{notes:?}");
        let rows = table(Some(file.labels()));
        let key = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .map(|row| row.key.to_string())
                .expect("a row")
        };
        assert_eq!(key("/model"), "alt+x m");
        assert_eq!(key("/level"), "alt+x t");
        assert_eq!(key("/sessions"), "alt+x l");
        assert_eq!(key("/mode"), "");
        assert_eq!(key("/help"), "?");
        assert_eq!(key("/quit"), "ctrl+c ctrl+c");
    }
}
