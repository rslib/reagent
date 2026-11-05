//! The command table: the names the `/` list shows and the palette filters.
//!
//! One table is built per app and shared by every list it opens, so the names,
//! the words and the keys a row draws are the ones the help sheet names.

use std::sync::Arc;

use super::list::Command;

/// The commands, in the order an empty list shows them.
const TABLE: [Command; 11] = [
    Command {
        name: "/model",
        words: "pick the model and the provider",
        key: "ctrl+x m",
        group: "model",
    },
    Command {
        name: "/level",
        words: "reasoning effort",
        key: "",
        group: "model",
    },
    Command {
        name: "/mode",
        words: "permission mode",
        key: "",
        group: "model",
    },
    Command {
        name: "/sessions",
        words: "open another session",
        key: "ctrl+x l",
        group: "session",
    },
    Command {
        name: "/new",
        words: "start a new session",
        key: "",
        group: "session",
    },
    Command {
        name: "/fork",
        words: "fork this session into a new one",
        key: "",
        group: "session",
    },
    Command {
        name: "/compact",
        words: "compact the history into a checkpoint",
        key: "",
        group: "session",
    },
    Command {
        name: "/login",
        words: "log in to a provider",
        key: "",
        group: "session",
    },
    Command {
        name: "/cost",
        words: "tokens and cost of this session",
        key: "",
        group: "view",
    },
    Command {
        name: "/help",
        words: "every key",
        key: "?",
        group: "view",
    },
    Command {
        name: "/quit",
        words: "leave the session",
        key: "ctrl+c ctrl+c",
        group: "app",
    },
];

/// The commands the app's lists open with, built once and shared.
pub(crate) fn table() -> Arc<[Command]> {
    Arc::from(TABLE.to_vec())
}
