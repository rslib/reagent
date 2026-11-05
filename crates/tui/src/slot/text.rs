//! The text of a component.

use std::sync::Arc;

/// Text a component carries: a literal, which costs no copy, or an [`Arc`] the
/// panel already holds, which a clone shares by count.
///
/// Panels that build their text once keep it in an `Arc<str>` and hand it over
/// with [`Text::shared`], so a draw returns the same text on every frame
/// without a copy. Panels whose text never changes pass a literal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Text {
    /// No text.
    #[default]
    Empty,
    /// A literal: no copy, no allocation.
    Static(&'static str),
    /// Text a panel holds: sharing it costs a count, not a copy.
    Shared(Arc<str>),
}

impl Text {
    /// The text.
    pub fn as_str(&self) -> &str {
        match self {
            Text::Empty => "",
            Text::Static(text) => text,
            Text::Shared(text) => text,
        }
    }

    /// True when there are no characters.
    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }

    /// How many bytes the text has.
    pub fn len(&self) -> usize {
        self.as_str().len()
    }

    /// The text of an `Arc` a panel already holds: a count, not a copy.
    pub fn shared(text: &Arc<str>) -> Self {
        Text::Shared(Arc::clone(text))
    }
}

impl AsRef<str> for Text {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&'static str> for Text {
    fn from(text: &'static str) -> Self {
        Text::Static(text)
    }
}

impl From<Arc<str>> for Text {
    fn from(text: Arc<str>) -> Self {
        Text::Shared(text)
    }
}

impl From<&Arc<str>> for Text {
    fn from(text: &Arc<str>) -> Self {
        Text::shared(text)
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Text::Shared(Arc::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_costs_no_allocation() {
        let text: Text = "a title".into();
        assert!(matches!(text, Text::Static("a title")));
        assert_eq!(text.as_str(), "a title");
        assert_eq!(text.len(), 7);
        assert!(!text.is_empty());
    }

    #[test]
    fn a_shared_text_keeps_one_buffer() {
        let owned: Arc<str> = Arc::from("a session");
        let text = Text::shared(&owned);
        assert_eq!(Arc::strong_count(&owned), 2);
        assert_eq!(text.as_str(), "a session");
        assert_eq!(Arc::strong_count(&owned), 2);
    }

    #[test]
    fn the_empty_text_reads_empty() {
        assert_eq!(Text::Empty.as_str(), "");
        assert!(Text::Empty.is_empty());
    }
}
