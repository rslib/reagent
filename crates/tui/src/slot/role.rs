//! The style role of a run of text.

/// What a run of text means, not how it looks.
///
/// A panel picks a role and the screen picks the colors, so no panel holds a
/// terminal color or a crossterm type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Role {
    /// Body text.
    #[default]
    Text,
    /// De-emphasized text: an age, a hint, the empty part of a bar.
    Dim,
    /// Emphasized text inside a line.
    Strong,
    /// A panel's title.
    Title,
    /// The accent: the focused title, the current row, the filled part of a bar.
    Accent,
    /// Something that worked.
    Good,
    /// Something to watch.
    Warn,
    /// Something that failed.
    Bad,
}
