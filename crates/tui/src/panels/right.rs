//! A value pushed to the right edge of a panel's row.
//!
//! A [`Component::Row`](crate::slot::Component::Row) writes its key, two spaces
//! and its value, so a value that reads at the panel's right edge, as `50k /
//! 200k` does under the `tokens` key, needs the spaces in front of it. The pad
//! depends on the width the panel is drawn at, which only the draw knows.

use std::cell::RefCell;
use std::sync::Arc;

use unicode_width::UnicodeWidthStr;

use crate::slot::Text;

/// A row value padded to the right edge of the row.
///
/// The text is built when the width or the value changes and kept otherwise, so
/// a steady draw allocates nothing and hands out the same `Arc<str>`.
#[derive(Debug, Default)]
pub(crate) struct Right {
    built: RefCell<Built>,
}

/// What the last build drew, and what the next one compares.
#[derive(Debug, Default)]
struct Built {
    /// The width the text was built for.
    width: u16,
    /// The value the text holds, so an unchanged row builds nothing.
    value: String,
    /// The spaces and the value, whose buffer keeps its capacity.
    line: String,
    /// The text the panel hands out.
    text: Text,
}

impl Right {
    /// The value, right-aligned under the key on a line of `width` columns.
    pub(crate) fn at(&self, key: &str, value: &Text, width: u16) -> Text {
        let mut built = self.built.borrow_mut();
        if built.width == width && built.value == value.as_str() {
            return built.text.clone();
        }
        let pad = usize::from(room(key, width)).saturating_sub(value.as_str().width());
        built.width = width;
        built.value.clear();
        built.value.push_str(value.as_str());
        built.line.clear();
        built.line.extend(std::iter::repeat_n(' ', pad));
        built.line.push_str(value.as_str());
        built.text = Text::Shared(Arc::from(built.line.as_str()));
        built.text.clone()
    }
}

/// The two columns the host indents a panel's rows by, under its title.
const INDENT: u16 = 2;

/// The columns a row's value has: the host gives every line but the first of a
/// panel two columns of indent, cuts the key to a third of what is left, then
/// writes two spaces before the value.
fn room(key: &str, width: u16) -> u16 {
    let width = width.saturating_sub(INDENT);
    let key = u16::try_from(key.width()).unwrap_or(u16::MAX);
    let used = key.min((width / 3).max(1)).min(width);
    width.saturating_sub(used).saturating_sub(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::{Component, Lines, Panel, Priority, Sidebar, Size, Update};

    /// A panel of one row, whose value the `Right` puts at the right edge.
    struct One {
        title: Text,
        value: Text,
        right: Right,
    }

    impl One {
        fn new(value: &'static str) -> One {
            One {
                title: Text::Static("One"),
                value: Text::Static(value),
                right: Right::default(),
            }
        }
    }

    impl Panel for One {
        fn id(&self) -> &str {
            "one"
        }

        fn title(&self) -> &Text {
            &self.title
        }

        fn priority(&self) -> Priority {
            1
        }

        fn min_width(&self) -> u16 {
            0
        }

        fn update(&mut self, _update: &Update<'_>) -> bool {
            false
        }

        fn draw(&self, size: Size, out: &mut Vec<Component>) {
            out.push(Component::row(
                "tokens",
                self.right.at("tokens", &self.value, size.width),
            ));
        }
    }

    /// The text of every line the host drew, runs joined.
    fn texts(lines: &Lines) -> Vec<String> {
        lines
            .iter()
            .map(|spans| spans.iter().map(|span| span.text()).collect())
            .collect()
    }

    #[test]
    fn the_value_ends_at_the_right_edge() {
        let mut sidebar = Sidebar::new();
        sidebar.add(One::new("50k / 200k"));
        let drawn = texts(sidebar.draw(28, 4));
        assert_eq!(drawn[1], "  tokens          50k / 200k", "{drawn:?}");
        assert_eq!(drawn[1].chars().count(), 28);
    }

    #[test]
    fn a_narrower_panel_moves_the_value_with_it() {
        let mut sidebar = Sidebar::new();
        sidebar.add(One::new("$0.42"));
        let drawn = texts(sidebar.draw(20, 4));
        assert_eq!(drawn[1], "  tokens       $0.42", "{drawn:?}");
        assert_eq!(drawn[1].chars().count(), 20);
    }

    #[test]
    fn a_value_wider_than_its_room_is_cut_by_the_host() {
        let mut sidebar = Sidebar::new();
        sidebar.add(One::new("a value far wider than the room it has"));
        let drawn = texts(sidebar.draw(20, 4));
        assert_eq!(drawn[1].chars().count(), 20, "{drawn:?}");
        assert!(drawn[1].starts_with("  tokens  "), "{drawn:?}");
    }

    #[test]
    fn the_same_width_and_value_build_nothing_again() {
        let right = Right::default();
        let value = Text::Static("$0.42");
        let first = right.at("cost", &value, 28);
        let capacity = right.built.borrow().line.capacity();
        let again = right.at("cost", &value, 28);
        assert_eq!(first.as_str(), again.as_str());
        assert_eq!(
            right.built.borrow().line.capacity(),
            capacity,
            "a second draw at the same width builds no text"
        );
        let moved = right.at("cost", &Text::Static("$1.00"), 40);
        assert_eq!(moved.as_str().chars().count(), 32, "{moved:?}");
        assert!(moved.as_str().ends_with("$1.00"), "{moved:?}");
    }

    #[test]
    fn a_key_wider_than_a_third_leaves_less_room() {
        assert_eq!(room("tokens", 28), 18);
        assert_eq!(room("a-very-long-key-of-many-columns", 28), 16);
        assert_eq!(room("k", 2), 0);
    }
}
