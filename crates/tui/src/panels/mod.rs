//! The built-in sidebar panels, written on the slot API only.
//!
//! A panel here says what its lines mean and nothing else: the host
//! ([`Sidebar`](crate::slot::Sidebar)) places it, folds it by its priority,
//! focuses it and scrolls it. Each panel builds the text of a line when its
//! data changes and hands it out with [`Text::shared`](crate::slot::Text) on
//! every draw, so a steady draw allocates nothing.
//!
//! [`add_builtin`] adds the four panels in sidebar order and in folding order:
//! Context, then Changes, then Sessions, then Clients. The context use is the
//! thing to keep when the sidebar is short; the client list is the first to
//! fold, then the folder's session list.
//!
//! The header row the sidebar folds into reads the same panels: the context
//! panel's share and cost, the changes panel's tally, the sessions that wait and
//! the client count. No panel data is kept twice.

mod changes;
mod clients;
mod context;
mod right;
mod sessions;

pub use changes::ChangesPanel;
pub use clients::ClientsPanel;
pub use context::ContextPanel;
pub(crate) use right::Right;
pub use sessions::{SessionRow, SessionsPanel};

use crate::slot::Sidebar;

/// Add the built-in panels to `sidebar`, folded last to first: Context,
/// Changes, Sessions, Clients. The app reaches one later with
/// [`Sidebar::get_mut`], such as `get_mut::<SessionsPanel>()`.
pub fn add_builtin(sidebar: &mut Sidebar) {
    sidebar.add(ContextPanel::new());
    sidebar.add(ChangesPanel::new());
    sidebar.add(SessionsPanel::new());
    sidebar.add(ClientsPanel::new());
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::slot::{Lines, Sidebar, Update};

    /// The text of every line the host drew, runs joined.
    fn texts(lines: &Lines) -> Vec<String> {
        lines
            .iter()
            .map(|spans| spans.iter().map(|span| span.text()).collect())
            .collect()
    }

    #[test]
    fn the_panels_draw_in_a_28_column_sidebar() {
        let now = 1_000_000;
        let mut sessions = SessionsPanel::new();
        sessions.set_rows(
            vec![
                SessionRow {
                    id: Arc::from("now"),
                    title: Arc::from("this session"),
                    updated: now - 60,
                    live: true,
                    waiting: false,
                    current: true,
                    headless: false,
                },
                SessionRow {
                    id: Arc::from("then"),
                    title: Arc::from("an older session"),
                    updated: now - 2 * 3_600,
                    live: false,
                    waiting: true,
                    current: false,
                    headless: false,
                },
            ],
            now,
        );
        let mut clients = ClientsPanel::new();
        clients.joined(2, "tmux:2");
        let mut sidebar = Sidebar::new();
        sidebar.add(ContextPanel::new());
        sidebar.add(ChangesPanel::new());
        sidebar.add(sessions);
        sidebar.add(clients);
        sidebar.update(&Update::Model {
            name: "a-model",
            window: 200_000,
        });
        sidebar.update(&Update::Usage {
            input: 50_000,
            output: 1_000,
        });
        sidebar.update(&Update::ToolCall {
            name: "write",
            arguments: r#"{"path":"crates/deep/src/a.rs","content":"one\ntwo"}"#,
        });
        let drawn = texts(sidebar.draw(28, 24));
        for line in &drawn {
            assert!(line.chars().count() <= 28, "{line:?} is too wide");
        }
        for wanted in [
            "Context",
            "a-model",
            "50k / 200k",
            "Changes",
            "a.rs",
            "+2",
            "Sessions",
            "● this session",
            "⏸ an older session",
            "Clients",
            "this terminal",
            "tmux:2",
            "watching",
        ] {
            assert!(
                drawn.iter().any(|line| line.contains(wanted)),
                "{wanted:?} is missing: {drawn:#?}"
            );
        }
    }

    #[test]
    fn the_builtin_panels_come_in_folding_order() {
        let mut sidebar = Sidebar::new();
        add_builtin(&mut sidebar);
        let panels: Vec<_> = (0..sidebar.len())
            .filter_map(|index| sidebar.panel(index))
            .collect();
        let ids: Vec<&str> = panels.iter().map(|panel| panel.id()).collect();
        assert_eq!(ids, ["context", "changes", "sessions", "clients"]);
        let titles: Vec<&str> = panels.iter().map(|panel| panel.title().as_str()).collect();
        assert_eq!(titles, ["Context", "Changes", "Sessions", "Clients"]);
        // Context keeps its content longest, Clients folds first.
        let priorities: Vec<u8> = panels.iter().map(|panel| panel.priority()).collect();
        assert!(
            priorities[0] > priorities[1] && priorities[1] > priorities[2],
            "{priorities:?}"
        );
        assert!(priorities[3] < priorities[2], "{priorities:?}");
        for panel in &panels {
            assert!(panel.min_width() > 0, "{} draws nowhere", panel.id());
        }
    }

    #[test]
    fn the_app_reaches_a_builtin_panel_by_its_type() {
        let mut sidebar = Sidebar::new();
        add_builtin(&mut sidebar);
        let rows = vec![SessionRow {
            id: Arc::from("s1"),
            title: Arc::from("this session"),
            updated: 100,
            live: true,
            waiting: false,
            current: true,
            headless: false,
        }];
        let sessions = sidebar
            .get_mut::<SessionsPanel>()
            .expect("a sessions panel");
        assert!(sessions.set_rows(rows, 100));
        assert!(sidebar.get_mut::<ContextPanel>().is_some());
        assert!(sidebar.get_mut::<ClientsPanel>().is_some());
        let drawn = texts(sidebar.draw(28, 24));
        assert!(
            drawn.iter().any(|line| line.contains("this session")),
            "{drawn:#?}"
        );
    }
}
