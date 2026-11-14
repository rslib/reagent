//! The Skills panel: the skills the session found, and the ones the model read.
//!
//! The panel reads no disk: the handshake's list arrives with
//! [`SkillsPanel::set`], and the calls of the session mark a skill with
//! [`SkillsPanel::read`] when one reads its file. The name and the scope of a
//! line are kept as they came, so a steady draw hands out the same `Arc<str>`s
//! and allocates nothing.
//!
//! A skill the model read in this session draws a `•` before its name, in the
//! accent role the other panels mark an active row with.

use std::path::Path;
use std::sync::Arc;

use crate::client::Skill;
use crate::slot::{Component, Panel, Priority, Role, Size, Text, Update};

/// The fold order: the lowest, so the skills fold before the built-in panels
/// when the sidebar is short. It sits under them in the sidebar too, and of two
/// equal priorities the host folds the one further down first.
const PRIORITY: Priority = 0;

/// The narrowest sidebar this panel draws in.
const MIN_WIDTH: u16 = 20;

/// The title of the panel.
const TITLE: &str = "Skills";

/// The mark of a skill the model read in this session.
const READ: &str = "•";

/// One skill as the panel draws it.
#[derive(Debug)]
struct Line {
    /// The absolute path of the skill's file, which finds the line again.
    path: Arc<str>,
    /// The name of the skill.
    name: Arc<str>,
    /// The folder the skill came from, as `global` or `project`.
    scope: Arc<str>,
    /// Whether the model read the skill in this session.
    read: bool,
}

/// The panel of the skills a session found.
#[derive(Debug)]
pub struct SkillsPanel {
    title: Text,
    lines: Vec<Line>,
}

impl SkillsPanel {
    /// A panel with no line, which the host hides until a list arrives.
    pub fn new() -> SkillsPanel {
        SkillsPanel {
            title: Text::Static(TITLE),
            lines: Vec::new(),
        }
    }

    /// Takes the skills the session found, in place of the ones it held.
    pub fn set(&mut self, skills: &[Skill]) {
        self.lines.clear();
        self.lines.reserve(skills.len());
        for skill in skills {
            self.lines.push(Line {
                path: Arc::clone(&skill.path),
                name: Arc::clone(&skill.name),
                scope: Arc::from(skill.scope.as_str()),
                read: false,
            });
        }
    }

    /// Marks the skill whose file is `path` read, and answers whether that
    /// changed the panel: the same skill read twice changes nothing, and a path
    /// no skill names marks none.
    pub fn read(&mut self, path: &Path) -> bool {
        for line in &mut self.lines {
            if Path::new(&*line.path) == path && !line.read {
                line.read = true;
                return true;
            }
        }
        false
    }

    /// The skills the panel lists, in the order the handshake named them: the
    /// name of each, the absolute path of its file and its scope, as the rows of
    /// the `/` list name them.
    pub fn skills(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.lines
            .iter()
            .map(|line| (&*line.name, &*line.path, &*line.scope))
    }

    /// How many skills the panel lists.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True when the panel lists no skill, and so draws nothing.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

impl Default for SkillsPanel {
    fn default() -> Self {
        SkillsPanel::new()
    }
}

impl Panel for SkillsPanel {
    fn id(&self) -> &str {
        "skills"
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

    fn hidden(&self) -> bool {
        self.lines.is_empty()
    }

    fn update(&mut self, _update: &Update<'_>) -> bool {
        false
    }

    fn draw(&self, _size: Size, out: &mut Vec<Component>) {
        for line in &self.lines {
            let (mark, role) = if line.read {
                (READ, Role::Accent)
            } else {
                ("", Role::Dim)
            };
            out.push(Component::item_marked(
                &line.name,
                mark,
                role,
                &line.scope,
                false,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Scope;
    use crate::slot::Sidebar;

    /// The skill `name`, whose file is `path`, from `scope`.
    fn skill(name: &str, path: &str, scope: Scope) -> Skill {
        Skill {
            name: Arc::from(name),
            path: Arc::from(path),
            scope,
        }
    }

    /// The components of the panel's draw.
    fn drawn(panel: &SkillsPanel) -> Vec<Component> {
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        out
    }

    /// The text of a line.
    fn text_of(component: &Component) -> &str {
        match component {
            Component::Item { text, .. } => text.as_str(),
            other => panic!("{other:?} is not a line"),
        }
    }

    /// The note of a line, which the scope draws in.
    fn note_of(component: &Component) -> &str {
        match component {
            Component::Item { note, .. } => note.as_str(),
            other => panic!("{other:?} is not a line"),
        }
    }

    /// The mark of a line and the role it draws in.
    fn mark_of(component: &Component) -> (&str, Role) {
        match component {
            Component::Item { mark, .. } => (mark.text().as_str(), mark.role()),
            other => panic!("{other:?} is not a line"),
        }
    }

    #[test]
    fn the_skills_panel_draws_a_line_for_each_skill_with_its_scope() {
        let mut panel = SkillsPanel::new();
        assert!(panel.is_empty());
        assert!(panel.hidden(), "a panel with no skill hides");
        panel.set(&[
            skill("demo", "/w/.agents/skills/demo/SKILL.md", Scope::Project),
            skill("tidy", "/c/reagent/skills/tidy/SKILL.md", Scope::Global),
        ]);
        assert_eq!(panel.len(), 2);
        assert!(!panel.hidden());
        assert_eq!(panel.title().as_str(), "Skills");
        let lines = drawn(&panel);
        assert_eq!(lines.len(), 2, "one line per skill");
        assert_eq!(text_of(&lines[0]), "demo");
        assert_eq!(note_of(&lines[0]), "project");
        assert_eq!(text_of(&lines[1]), "tidy");
        assert_eq!(note_of(&lines[1]), "global");
        assert_eq!(mark_of(&lines[0]), ("", Role::Dim), "no mark before a read");
    }

    #[test]
    fn a_skills_read_marks_its_line_and_a_second_read_changes_nothing() {
        let mut panel = SkillsPanel::new();
        panel.set(&[
            skill("demo", "/w/.agents/skills/demo/SKILL.md", Scope::Project),
            skill("tidy", "/c/reagent/skills/tidy/SKILL.md", Scope::Global),
        ]);
        let path = Path::new("/w/.agents/skills/demo/SKILL.md");
        assert!(panel.read(path), "the read marks the line");
        let lines = drawn(&panel);
        assert_eq!(mark_of(&lines[0]), (READ, Role::Accent));
        assert_eq!(mark_of(&lines[1]), ("", Role::Dim), "the other line stays");
        let kept = text_of(&lines[0]).as_ptr();
        assert!(!panel.read(path), "a second read changes nothing");
        let again = drawn(&panel);
        assert_eq!(format!("{:?}", again[0]), format!("{:?}", lines[0]));
        assert_eq!(
            text_of(&again[0]).as_ptr(),
            kept,
            "the second mark builds no new line text"
        );
        // A path no skill names marks none of them.
        assert!(!panel.read(Path::new("/w/other/SKILL.md")));
        assert!(!panel.read(Path::new("/c/reagent/skills/other/SKILL.md")));
    }

    #[test]
    fn a_skills_panel_folds_first_of_the_sidebar() {
        let mut sidebar = Sidebar::new();
        crate::panels::add_builtin(&mut sidebar);
        sidebar.add(SkillsPanel::new());
        let last = sidebar.len() - 1;
        let skills = sidebar
            .panel(last)
            .expect("the skills panel sits last")
            .priority();
        let builtin: Vec<Priority> = (0..last)
            .filter_map(|index| sidebar.panel(index))
            .map(|panel| panel.priority())
            .collect();
        assert!(
            builtin.iter().all(|other| *other >= skills),
            "{builtin:?} keeps its content longer than {skills}"
        );
    }
}
