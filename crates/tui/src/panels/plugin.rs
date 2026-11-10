//! The panel of a JS plugin: the title and the lines its worker sent.
//!
//! The worker sends one `plugin_panel` notice per draw of a plugin's panel, so
//! the lines are turned into components once, when the notice arrives: a frame
//! only shares them.

use std::sync::Arc;

use reagent_protocol::message::PanelLine;

use crate::slot::{Component, Panel, Priority, Role, Size, Text, Update};

/// The fold order: the lowest, so a plugin panel folds before every built-in
/// panel when the sidebar is short. It sits below them in the sidebar too, and
/// of two equal priorities the host folds the one further down first.
const PRIORITY: Priority = 0;

/// The lines a panel keeps: a plugin that sends more has the rest cut, so no
/// panel can hold the sidebar to itself.
pub const MAX_LINES: usize = 50;

/// The panel one JS plugin sets.
#[derive(Debug)]
pub struct PluginPanel {
    id: String,
    title: Text,
    /// The components of the last notice, one per line, at most [`MAX_LINES`].
    lines: Vec<Component>,
}

impl PluginPanel {
    /// A panel known by `id`, with no title and no line: the host hides it
    /// until a notice gives it a line.
    pub fn new(id: &str) -> PluginPanel {
        PluginPanel {
            id: id.to_owned(),
            title: Text::Empty,
            lines: Vec::new(),
        }
    }

    /// Takes the title and the lines of a notice, in place of the ones it held.
    pub fn set(&mut self, title: &str, lines: &[PanelLine<'_>]) {
        self.title = Text::Shared(Arc::from(title));
        self.lines.clear();
        self.lines
            .extend(lines.iter().take(MAX_LINES).map(component));
    }

    /// How many lines the panel holds.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True when the panel holds no line, and so draws nothing.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// The component of one line of a notice.
fn component(line: &PanelLine<'_>) -> Component {
    match line {
        PanelLine::Plain(text) => Component::Text {
            text: Text::Shared(Arc::from(text.as_ref())),
            role: Role::Text,
        },
        PanelLine::Text { text, role } => Component::Text {
            text: Text::Shared(Arc::from(text.as_ref())),
            role: role_of(role.as_deref()),
        },
        PanelLine::Row { key, value } => Component::row(
            Text::Shared(Arc::from(key.as_ref())),
            Text::Shared(Arc::from(value.as_ref())),
        ),
        PanelLine::Progress { label, value, max } => {
            Component::progress(Text::Shared(Arc::from(label.as_ref())), *value, *max)
        }
    }
}

/// The role a notice names, and the body role for a role this build does not
/// know.
fn role_of(role: Option<&str>) -> Role {
    match role {
        Some("dim") => Role::Dim,
        Some("strong") => Role::Strong,
        Some("accent") => Role::Accent,
        Some("good") => Role::Good,
        Some("warn") => Role::Warn,
        Some("bad") => Role::Bad,
        _ => Role::Text,
    }
}

impl Panel for PluginPanel {
    fn id(&self) -> &str {
        &self.id
    }

    fn title(&self) -> &Text {
        &self.title
    }

    fn priority(&self) -> Priority {
        PRIORITY
    }

    fn min_width(&self) -> u16 {
        0
    }

    fn hidden(&self) -> bool {
        self.lines.is_empty()
    }

    fn update(&mut self, _update: &Update<'_>) -> bool {
        false
    }

    fn draw(&self, _size: Size, out: &mut Vec<Component>) {
        out.extend(self.lines.iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    /// The text of every line the panel drew.
    fn drawn(panel: &PluginPanel) -> Vec<String> {
        let mut out = Vec::new();
        panel.draw(Size::new(20, 10), &mut out);
        out.iter()
            .map(|component| match component {
                Component::Text { text, .. } => text.as_str().to_owned(),
                Component::Row { key, value } => {
                    format!("{} {}", key.as_str(), value.text().as_str())
                }
                Component::Progress { label, value, max } => {
                    format!("{} {value}/{max}", label.as_str())
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn every_line_shape_draws_its_text() {
        let mut panel = PluginPanel::new("myplug/build");
        assert!(panel.hidden(), "a panel with no line hides");
        let lines = vec![
            PanelLine::Plain(Cow::Borrowed("plain")),
            PanelLine::Text {
                text: Cow::Borrowed("dim"),
                role: Some(Cow::Borrowed("dim")),
            },
            PanelLine::Text {
                text: Cow::Borrowed("odd"),
                role: Some(Cow::Borrowed("chartreuse")),
            },
            PanelLine::Row {
                key: Cow::Borrowed("branch"),
                value: Cow::Borrowed("main"),
            },
            PanelLine::Progress {
                label: Cow::Borrowed("tests"),
                value: 3,
                max: 10,
            },
        ];
        panel.set("Build", &lines);
        assert_eq!(panel.title().as_str(), "Build");
        assert!(!panel.hidden(), "a panel with a line shows");
        assert_eq!(
            drawn(&panel),
            ["plain", "dim", "odd", "branch main", "tests 3/10"]
        );
        let roles: Vec<Role> = {
            let mut out = Vec::new();
            panel.draw(Size::new(20, 10), &mut out);
            out.iter()
                .filter_map(|component| match component {
                    Component::Text { role, .. } => Some(*role),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(roles, [Role::Text, Role::Dim, Role::Text]);
        // A later notice replaces the lines it held.
        panel.set("Build", &[PanelLine::Plain(Cow::Borrowed("done"))]);
        assert_eq!(drawn(&panel), ["done"]);
        // No line hides the panel again, and its title goes with it.
        panel.set("Build", &[]);
        assert!(panel.hidden());
        assert!(drawn(&panel).is_empty());
    }

    /// Every role a notice may name picks its own role, and a role this build
    /// does not know, or one it does not name, picks the body's.
    #[test]
    fn a_line_role_maps_to_its_role() {
        for (named, role) in [
            (Some("text"), Role::Text),
            (Some("dim"), Role::Dim),
            (Some("strong"), Role::Strong),
            (Some("accent"), Role::Accent),
            (Some("good"), Role::Good),
            (Some("warn"), Role::Warn),
            (Some("bad"), Role::Bad),
            (Some("chartreuse"), Role::Text),
            (Some("Dim"), Role::Text),
            (None, Role::Text),
        ] {
            assert_eq!(role_of(named), role, "{named:?}");
        }
        // The key of a row carries its own weight and the value is body text.
        let row = component(&PanelLine::Row {
            key: Cow::Borrowed("branch"),
            value: Cow::Borrowed("main"),
        });
        let Component::Row { value, .. } = row else {
            panic!("a row: {row:?}");
        };
        assert_eq!(value.role(), Role::Text);
        assert_eq!(value.as_str(), "main");
    }

    #[test]
    fn a_panel_keeps_at_most_its_cap_of_lines() {
        let lines: Vec<PanelLine<'_>> = (0..80)
            .map(|index| PanelLine::Plain(Cow::Owned(format!("line {index}"))))
            .collect();
        let mut panel = PluginPanel::new("many");
        panel.set("Many", &lines);
        assert_eq!(panel.len(), MAX_LINES);
        let drawn = drawn(&panel);
        assert_eq!(drawn.len(), MAX_LINES);
        assert_eq!(drawn[0], "line 0");
        assert_eq!(drawn[MAX_LINES - 1], format!("line {}", MAX_LINES - 1));
    }

    #[test]
    fn a_plugin_panel_folds_before_every_builtin_panel() {
        let mut sidebar = crate::slot::Sidebar::new();
        crate::panels::add_builtin(&mut sidebar);
        sidebar.add(PluginPanel::new("myplug/build"));
        let plugin = sidebar
            .get_mut_by_id::<PluginPanel>("myplug/build")
            .expect("the plugin panel");
        plugin.set("Build", &[PanelLine::Plain(Cow::Borrowed("a line"))]);
        let priorities: Vec<Priority> = (0..sidebar.len())
            .filter_map(|index| sidebar.panel(index))
            .map(|panel| panel.priority())
            .collect();
        let plugin = *priorities.last().expect("the plugin panel");
        assert!(
            priorities[..priorities.len() - 1]
                .iter()
                .all(|builtin| *builtin >= plugin),
            "{priorities:?}"
        );
        // It sits last, so of the lowest priority it folds first.
        assert_eq!(sidebar.len(), 5);
        assert!(sidebar.get_mut_by_id::<PluginPanel>("none").is_none());
    }
}
