//! The Clients panel: this terminal and the other clients on the session.
//!
//! The panel reads no socket: the app takes the worker's `client_joined` and
//! `client_left` notices and hands the client's name over with
//! [`ClientsPanel::joined`] and [`ClientsPanel::left`]. The count the header row
//! reads is built when a client joins or leaves, so a draw allocates nothing.

use std::sync::Arc;

use crate::slot::{Component, Panel, Priority, Role, Size, Text, Update};

/// The fold order: the client list folds before the session list, because a
/// session is what the user came for.
const PRIORITY: Priority = 0;

/// The narrowest sidebar this panel draws in.
const MIN_WIDTH: u16 = 20;

/// The label of the terminal the app runs in.
const YOU: &str = "this terminal";

/// What the app calls the row of this terminal.
const YOURS: &str = "you";

/// The dot that marks this terminal, drawn in the accent role.
const THIS: &str = "●";

/// What another client does with the session: it watches it.
const WATCHING: &str = "watching";

/// The Clients panel.
#[derive(Debug)]
pub struct ClientsPanel {
    title: Text,
    /// The other clients, in the order they joined, by the id the worker gave
    /// them and the name they chose.
    others: Vec<(u64, Arc<str>)>,
    /// `2 clients`, which the header row reads.
    count: Text,
}

impl ClientsPanel {
    /// A panel that shows this terminal alone.
    pub fn new() -> ClientsPanel {
        ClientsPanel {
            title: Text::Static("Clients"),
            others: Vec::new(),
            count: Text::Static("1 client"),
        }
    }

    /// Take the client that joined the session: its id, which a later
    /// `client_left` names, and the name it chose. Answers false for a client
    /// that was already there, or one that gave no name.
    pub fn joined(&mut self, id: u64, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.others.iter().any(|(client, _)| *client == id) {
            return false;
        }
        self.others.push((id, Arc::from(name)));
        self.build_count();
        true
    }

    /// Take a client that left the session, by the id the notice names. Answers
    /// false for one the panel does not show.
    pub fn left(&mut self, id: u64) -> bool {
        let Some(index) = self.others.iter().position(|(client, _)| *client == id) else {
            return false;
        };
        self.others.remove(index);
        self.build_count();
        true
    }

    /// How many clients show, this terminal included.
    pub fn clients(&self) -> usize {
        self.others.len() + 1
    }

    /// The count as the header row reads it: `2 clients`.
    pub fn count(&self) -> &Text {
        &self.count
    }

    /// Build the count the header row shows, which changes only with a join or
    /// a leave.
    fn build_count(&mut self) {
        self.count = if self.others.is_empty() {
            Text::Static("1 client")
        } else {
            Text::Shared(Arc::from(format!("{} clients", self.clients())))
        };
    }
}

impl Default for ClientsPanel {
    fn default() -> Self {
        ClientsPanel::new()
    }
}

impl Panel for ClientsPanel {
    fn id(&self) -> &str {
        "clients"
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

    fn update(&mut self, _update: &Update<'_>) -> bool {
        false
    }

    fn draw(&self, _size: Size, out: &mut Vec<Component>) {
        out.push(Component::item_marked(YOU, THIS, Role::Accent, YOURS, true));
        for (_, client) in &self.others {
            out.push(Component::item_with(Text::shared(client), WATCHING, false));
        }
        // One blank row between the panels.
        out.push(Component::text(Text::Static("")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(panel: &ClientsPanel) -> Vec<Component> {
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
    fn this_terminal_shows_a_dot_in_the_accent_role() {
        let mut panel = ClientsPanel::new();
        let out = drawn(&panel);
        let Component::Item { mark, .. } = &out[0] else {
            panic!("{:?} is not a row", out[0]);
        };
        assert_eq!(mark.text().as_str(), THIS);
        assert_eq!(
            mark.role(),
            Role::Accent,
            "the dot marks this terminal in the accent"
        );
        assert!(panel.joined(2, "tmux:2"));
        let out = drawn(&panel);
        let Component::Item { mark, .. } = &out[1] else {
            panic!("{:?} is not a row", out[1]);
        };
        assert!(mark.is_empty(), "another client carries no dot");
    }

    #[test]
    fn this_terminal_shows_alone_until_a_client_joins() {
        let panel = ClientsPanel::new();
        let out = drawn(&panel);
        assert_eq!(out.len(), 2, "this terminal and the blank row: {out:?}");
        assert_eq!(text_of(&out[0]), YOU);
        assert_eq!(note_of(&out[0]), YOURS);
        assert!(out[0].is_selected(), "this terminal is the current client");
        assert_eq!(panel.clients(), 1);
        assert_eq!(panel.count().as_str(), "1 client");
    }

    #[test]
    fn a_joined_client_shows_with_watching_and_leaves_again() {
        let mut panel = ClientsPanel::new();
        assert!(panel.joined(2, "tmux:2"));
        let out = drawn(&panel);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(text_of(&out[1]), "tmux:2");
        assert_eq!(note_of(&out[1]), WATCHING);
        assert!(!out[1].is_selected());
        assert_eq!(panel.count().as_str(), "2 clients");
        assert!(panel.left(2));
        let out = drawn(&panel);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(text_of(&out[0]), YOU);
        assert_eq!(panel.count().as_str(), "1 client");
    }

    #[test]
    fn a_client_that_is_there_or_not_is_one_answer() {
        let mut panel = ClientsPanel::new();
        assert!(panel.joined(2, "one"));
        assert!(!panel.joined(2, "one"), "a second join changes nothing");
        assert!(panel.joined(3, "two"), "a second client has its own id");
        assert!(panel.left(2), "the leave names the id");
        assert!(!panel.left(2), "a second leave changes nothing");
        assert!(!panel.joined(4, "  "), "a client with no name is no client");
        assert_eq!(panel.clients(), 2);
    }

    #[test]
    fn an_ignored_update_changes_nothing() {
        let mut panel = ClientsPanel::new();
        panel.joined(2, "one");
        let before = rendered(&drawn(&panel));
        for update in [
            Update::Usage {
                input: 1,
                output: 2,
            },
            Update::Busy(true),
            Update::Tick,
        ] {
            assert!(!panel.update(&update), "{update:?}");
        }
        assert_eq!(rendered(&drawn(&panel)), before);
        assert!(!panel.wants_tick());
    }

    #[test]
    fn a_second_draw_keeps_its_buffer_and_returns_equal_components() {
        let mut panel = ClientsPanel::new();
        panel.joined(2, "one");
        panel.joined(3, "two");
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
}
