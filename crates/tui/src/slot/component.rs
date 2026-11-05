//! The semantic components a panel fills its draw buffer with.

use super::{Role, Text};

/// A run of a component: text in one role.
///
/// A mark before a list row is the one run a panel supplies itself; the rest of
/// the line the host places. The text is a [`Text`], so a panel that already
/// holds its text shares it rather than copying it.
#[derive(Debug, Clone)]
pub struct Part {
    text: Text,
    role: Role,
}

impl Part {
    /// A run of `text` in `role`.
    pub(crate) fn new(text: impl Into<Text>, role: Role) -> Part {
        Part {
            text: text.into(),
            role,
        }
    }

    /// The text.
    pub(crate) fn text(&self) -> &Text {
        &self.text
    }

    /// The role that picks its style.
    pub(crate) fn role(&self) -> Role {
        self.role
    }

    /// The text as one string, which a test compares.
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        self.text.as_str()
    }

    /// True when the run draws nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

impl Default for Part {
    fn default() -> Part {
        Part {
            text: Text::Empty,
            role: Role::Dim,
        }
    }
}

/// The right-aligned note of a list row: one text, in one or two roles.
///
/// A note such as `+14 -3` draws its added count in the add role and its
/// removed count in the delete role: `split` is the byte where the second role
/// starts, and it is the text's length for a note of one role.
#[derive(Debug, Clone)]
pub struct Note {
    text: Text,
    split: u32,
    first: Role,
    second: Role,
}

impl Note {
    /// A note of one role.
    pub(crate) fn new(text: impl Into<Text>, role: Role) -> Note {
        let text = text.into();
        Note {
            split: text.len() as u32,
            text,
            first: role,
            second: role,
        }
    }

    /// A note whose bytes before `split` are in `first` and the rest in
    /// `second`.
    pub(crate) fn two(text: impl Into<Text>, split: usize, first: Role, second: Role) -> Note {
        let text = text.into();
        Note {
            split: (split as u32).min(text.len() as u32),
            text,
            first,
            second,
        }
    }

    /// The text.
    pub(crate) fn text(&self) -> &Text {
        &self.text
    }

    /// The note as one string, which the tests compare.
    pub(crate) fn as_str(&self) -> &str {
        self.text.as_str()
    }

    /// The byte where the second role starts.
    pub(crate) fn split(&self) -> u32 {
        self.split
    }

    /// The role of the run before [`Note::split`].
    pub(crate) fn first(&self) -> Role {
        self.first
    }

    /// The role of the run from [`Note::split`].
    pub(crate) fn second(&self) -> Role {
        self.second
    }
}

impl Default for Note {
    fn default() -> Note {
        Note {
            text: Text::Empty,
            split: 0,
            first: Role::Dim,
            second: Role::Dim,
        }
    }
}

/// One piece of a panel, as the panel means it.
///
/// A component is one line of a panel: the host decides where it goes, how wide
/// it is and how it looks, and turns it into styled spans. A panel never sees a
/// cell, a screen or a crossterm type.
#[derive(Debug, Clone)]
pub enum Component {
    /// One line of text in one role.
    Text {
        /// The text.
        text: Text,
        /// The role.
        role: Role,
    },
    /// One row of a list: the text, a mark before it, a note after it, and
    /// whether it is the current row.
    Item {
        /// The row's text.
        text: Text,
        /// A mark before the text in its own role, such as a dot; empty draws
        /// the host's own current-row marker.
        mark: Part,
        /// A note right-aligned after the text, such as an age; empty for none.
        note: Note,
        /// Whether this is the current row.
        selected: bool,
        /// Whether the text draws dim, as a row a headless run wrote does.
        dim: bool,
    },
    /// A key and its value on one line, such as `tokens 50k / 200k`.
    Row {
        /// The key.
        key: Text,
        /// The value, in its own role.
        value: Part,
    },
    /// A labelled progress bar: `value` of `max`, with `max` of zero drawn empty.
    Progress {
        /// The label before the bar.
        label: Text,
        /// How far the bar is.
        value: u64,
        /// How far it goes.
        max: u64,
    },
    /// A short mark for a row, such as `approval`, in one role.
    Badge {
        /// The mark.
        text: Text,
        /// The role, which picks the color.
        role: Role,
    },
}

impl Component {
    /// A line of body text.
    pub fn text(text: impl Into<Text>) -> Component {
        Component::Text {
            text: text.into(),
            role: Role::Text,
        }
    }

    /// A line of de-emphasized text.
    pub fn dim(text: impl Into<Text>) -> Component {
        Component::Text {
            text: text.into(),
            role: Role::Dim,
        }
    }

    /// A line of emphasized text.
    pub fn strong(text: impl Into<Text>) -> Component {
        Component::Text {
            text: text.into(),
            role: Role::Strong,
        }
    }

    /// A list row with no note, and no mark of its own.
    pub fn item(text: impl Into<Text>, selected: bool) -> Component {
        Component::Item {
            text: text.into(),
            mark: Part::default(),
            note: Note::default(),
            selected,
            dim: false,
        }
    }

    /// A list row with a note right-aligned after its text, in the dim role.
    pub fn item_with(text: impl Into<Text>, note: impl Into<Text>, selected: bool) -> Component {
        Component::Item {
            text: text.into(),
            mark: Part::default(),
            note: Note::new(note, Role::Dim),
            selected,
            dim: false,
        }
    }

    /// A list row with a mark of its own before the text, such as a client's
    /// dot, and a note in the dim role. The mark's text sits in `mark_role`.
    pub fn item_marked(
        text: impl Into<Text>,
        mark: &'static str,
        mark_role: Role,
        note: impl Into<Text>,
        selected: bool,
    ) -> Component {
        Component::Item {
            text: text.into(),
            mark: Part::new(mark, mark_role),
            note: Note::new(note, Role::Dim),
            selected,
            dim: false,
        }
    }

    /// A list row whose note draws its bytes before `split` in the add role and
    /// the rest in the delete role, as a changes tally `+14 -3` reads.
    pub fn item_scored(
        text: impl Into<Text>,
        note: impl Into<Text>,
        split: usize,
        selected: bool,
    ) -> Component {
        Component::Item {
            text: text.into(),
            mark: Part::default(),
            note: Note::two(note, split, Role::Good, Role::Bad),
            selected,
            dim: false,
        }
    }

    /// A list row whose note draws its bytes before `split` in `first` and the
    /// rest in `second`, as a footer's two arrows read when only one of them
    /// has a page to turn to.
    pub fn item_split(
        text: impl Into<Text>,
        note: impl Into<Text>,
        split: usize,
        first: Role,
        second: Role,
        selected: bool,
    ) -> Component {
        Component::Item {
            text: text.into(),
            mark: Part::default(),
            note: Note::two(note, split, first, second),
            selected,
            dim: false,
        }
    }

    /// The same component with the text of a list row drawn dim, as the row of
    /// a session a headless run wrote reads. Any other component is unchanged.
    pub fn dimmed(mut self) -> Component {
        if let Component::Item { dim, .. } = &mut self {
            *dim = true;
        }
        self
    }

    /// A key and its value, the value in the body role.
    pub fn row(key: impl Into<Text>, value: impl Into<Text>) -> Component {
        Component::row_with(key, value, Role::Text)
    }

    /// A key and its value in its own role.
    pub fn row_with(key: impl Into<Text>, value: impl Into<Text>, role: Role) -> Component {
        Component::Row {
            key: key.into(),
            value: Part::new(value, role),
        }
    }

    /// A labelled bar of `value` of `max`.
    pub fn progress(label: impl Into<Text>, value: u64, max: u64) -> Component {
        Component::Progress {
            label: label.into(),
            value,
            max,
        }
    }

    /// A short mark in one role.
    pub fn badge(text: impl Into<Text>, role: Role) -> Component {
        Component::Badge {
            text: text.into(),
            role,
        }
    }

    /// Whether the component is a list row that is the current one.
    pub fn is_selected(&self) -> bool {
        match self {
            Component::Item { selected, .. } => *selected,
            _ => false,
        }
    }
}
