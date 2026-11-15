//! The shared reasoning scale: the named levels every provider's words map
//! to, and the reasoning plan of one model.

use std::borrow::Cow;

use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};

/// A named reasoning level, in ascending order: `minimal` thinks least and
/// `max` most.
///
/// The plan of a model lives here rather than in `types`, which already calls
/// the text of a request [`crate::Reasoning`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// The least thinking.
    Minimal,
    /// A little thinking.
    Low,
    /// The usual middle.
    Medium,
    /// More than the middle.
    High,
    /// Extra high, as OpenAI names it.
    Xhigh,
    /// The most.
    Max,
}

impl Level {
    /// Every level, lowest first.
    pub const ALL: [Level; 6] = [
        Level::Minimal,
        Level::Low,
        Level::Medium,
        Level::High,
        Level::Xhigh,
        Level::Max,
    ];

    /// The level `word` names, ignoring case and surrounding space, or `None`
    /// for a word reagent does not know.
    pub fn parse(word: &str) -> Option<Level> {
        let word = word.trim();
        Level::ALL
            .into_iter()
            .find(|level| level.name().eq_ignore_ascii_case(word))
    }

    /// The word a provider writes for this level, which the cache file and the
    /// start line use too.
    pub const fn name(self) -> &'static str {
        match self {
            Level::Minimal => "minimal",
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
            Level::Xhigh => "xhigh",
            Level::Max => "max",
        }
    }

    /// This level's bit in a [`Levels`] set.
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl Serialize for Level {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Level {
    /// An unknown word is an error here: reagent writes only names it knows, so
    /// a bad word is a bad field, which leaves the cache unreadable like any
    /// other.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        Level::parse(&word)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown level: {word}")))
    }
}

/// The named levels a model accepts, in one byte that writes to the cache file
/// as the list of names, lowest first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Levels(u8);

impl Levels {
    /// Adds `level` to the set.
    pub fn insert(&mut self, level: Level) {
        self.0 |= level.bit();
    }

    /// Whether the set holds `level`.
    pub fn contains(self, level: Level) -> bool {
        self.0 & level.bit() != 0
    }

    /// Whether the set holds no level.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The levels of the set, lowest first.
    pub fn iter(self) -> impl Iterator<Item = Level> {
        Level::ALL
            .into_iter()
            .filter(move |level| self.contains(*level))
    }

    /// The lowest level of the set.
    pub fn lowest(self) -> Option<Level> {
        self.iter().next()
    }

    /// The lowest level of the set above `level`.
    pub fn above(self, level: Level) -> Option<Level> {
        self.iter().find(|each| *each > level)
    }

    /// The highest level of the set below `level`.
    pub fn below(self, level: Level) -> Option<Level> {
        Level::ALL
            .into_iter()
            .rfind(|each| self.contains(*each) && *each < level)
    }
}

impl Serialize for Levels {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.iter().count()))?;
        for level in self.iter() {
            seq.serialize_element(level.name())?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Levels {
    /// A word reagent does not know is skipped: a source may name a level it
    /// has not met yet.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut levels = Levels::default();
        for word in Vec::<String>::deserialize(deserializer)? {
            if let Some(level) = Level::parse(&word) {
                levels.insert(level);
            }
        }
        Ok(levels)
    }
}

/// What a model takes as its reasoning: the levels it accepts, whether thinking
/// can be turned off and must be on, the level it starts at and the words it
/// lists that reagent does not know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    /// The named levels the model accepts, lowest first.
    pub levels: Levels,
    /// Whether thinking can be turned off, or `None` when the source does not
    /// say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off: Option<bool>,
    /// Whether thinking cannot be turned off at all.
    #[serde(default)]
    pub mandatory: bool,
    /// The level the model starts at, when the source says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Level>,
    /// The words the source lists that are neither a known level nor an off
    /// word, each once, as first written. reagent cannot order them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
    /// Whether the model does not think at all, so a request names no
    /// reasoning: an endpoint may refuse the field, even with its off word.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub never: bool,
}

impl Reasoning {
    /// The plan of a model that does not think.
    pub fn never() -> Reasoning {
        Reasoning {
            never: true,
            ..Reasoning::default()
        }
    }

    /// The plan the words of one source describe: the levels it names, with
    /// `off` or `none` turning thinking off. A word reagent does not know is
    /// kept in `unknown`.
    pub fn from_words<'a>(words: impl IntoIterator<Item = &'a str>) -> Reasoning {
        let mut reasoning = Reasoning::default();
        for word in words {
            reasoning.add(word);
        }
        reasoning
    }

    /// Adds what one word of a source says.
    fn add(&mut self, word: &str) {
        let word = word.trim();
        match Level::parse(word) {
            Some(level) => self.levels.insert(level),
            None if is_off(word) => self.off = Some(true),
            None if word.is_empty() || self.takes_unknown(word) => {}
            None => self.unknown.push(word.to_string()),
        }
    }

    /// Whether `word` is one of the words reagent does not know that the
    /// model lists.
    pub fn takes_unknown(&self, word: &str) -> bool {
        let word = word.trim();
        self.unknown
            .iter()
            .any(|known| known.eq_ignore_ascii_case(word))
    }

    /// Whether the plan says nothing at all.
    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
            && self.off.is_none()
            && !self.mandatory
            && self.default.is_none()
            && self.unknown.is_empty()
            && !self.never
    }
}

/// Whether a source's word turns thinking off.
fn is_off(word: &str) -> bool {
    let word = word.trim();
    word.eq_ignore_ascii_case("off") || word.eq_ignore_ascii_case("none")
}

/// One level word as a list writes it, borrowed from the reply when it holds
/// no escape. serde borrows a `Cow` only as a direct field, never inside a
/// `Vec` or an `Option`, so lists of words hold this instead.
#[derive(Deserialize)]
pub(crate) struct Word<'a>(#[serde(borrow)] Cow<'a, str>);

impl Word<'_> {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A list of level words read straight into a plan: each word is borrowed
/// from the reply and read, and no list of them is kept.
#[derive(Default)]
pub(crate) struct Words(pub(crate) Reasoning);

impl<'de> Deserialize<'de> for Words {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct List;

        impl<'de> serde::de::Visitor<'de> for List {
            type Value = Words;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a list of level words")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Words, A::Error> {
                let mut reasoning = Reasoning::default();
                while let Some(word) = seq.next_element::<Word<'de>>()? {
                    reasoning.add(word.as_str());
                }
                Ok(Words(reasoning))
            }
        }

        deserializer.deserialize_seq(List)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_words_are_kept_once_and_known_ones_are_not() {
        let plan = Reasoning::from_words(["low", "ultra", " high", "Ultra", ""]);
        assert_eq!(
            plan.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::High]
        );
        assert_eq!(plan.unknown, ["ultra"]);
        assert!(!plan.is_empty());
        assert!(plan.takes_unknown(" ULTRA "));
        assert!(!plan.takes_unknown("high"));

        let known = Reasoning::from_words(["low", "none"]);
        assert!(known.unknown.is_empty());
        let json = serde_json::to_string(&known).unwrap();
        assert!(!json.contains("unknown"), "{json}");
        let back: Reasoning = serde_json::from_str(&json).unwrap();
        assert_eq!(back, known);
    }

    #[test]
    fn unknown_words_alone_are_a_plan() {
        let plan = Reasoning::from_words(["deep"]);
        assert!(plan.levels.is_empty());
        assert!(!plan.is_empty());
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<Reasoning>(&json).unwrap(), plan);
    }

    #[test]
    fn word_borrows_from_the_reply_in_a_list_and_an_option() {
        let words: Vec<Word<'_>> = serde_json::from_str(r#"["low","high"]"#).unwrap();
        assert!(words.iter().all(|word| matches!(word.0, Cow::Borrowed(_))));
        let word: Option<Word<'_>> = serde_json::from_str(r#""max""#).unwrap();
        assert!(matches!(
            word.map(|word| word.0),
            Some(Cow::Borrowed("max"))
        ));
    }

    #[test]
    fn reasoning_levels_above_and_below_walk_the_set() {
        let plan = Reasoning::from_words(["low", "medium", "xhigh"]);
        assert_eq!(plan.levels.lowest(), Some(Level::Low));
        assert_eq!(plan.levels.above(Level::Low), Some(Level::Medium));
        assert_eq!(plan.levels.above(Level::Medium), Some(Level::Xhigh));
        assert_eq!(plan.levels.above(Level::Xhigh), None);
        assert_eq!(plan.levels.below(Level::Xhigh), Some(Level::Medium));
        assert_eq!(plan.levels.below(Level::Low), None);
        assert!(Reasoning::default().is_empty());
        assert!(!plan.is_empty());
    }
}
