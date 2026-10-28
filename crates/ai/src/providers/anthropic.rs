//! The anthropic preset.

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{Preset, Route, Wire};
use crate::catalog::{Entry, shaped};
use crate::levels::{Level, Reasoning};

pub const PRESET: Preset = Preset {
    name: "anthropic",
    routes: &[Route {
        name: "messages",
        wire: Wire::Anthropic,
        base_url: "https://api.anthropic.com",
        base_url_env: Some("ANTHROPIC_BASE_URL"),
        ..Route::DEFAULT
    }],
    api_key_env: "ANTHROPIC_API_KEY",
    read_list,
    ..Preset::DEFAULT
};

/// One entry of Anthropic's own model list: an id and a `capabilities` object
/// naming the effort levels and the thinking types the model supports.
#[derive(Deserialize)]
struct Item<'a> {
    id: String,
    /// The `capabilities` object, kept raw until the entry is built, so one odd
    /// model does not fail the whole list.
    #[serde(default, borrow)]
    capabilities: Option<&'a RawValue>,
}

impl<'a> From<Item<'a>> for Entry {
    fn from(item: Item<'a>) -> Entry {
        Entry {
            id: item.id,
            context_window: None,
            max_output: None,
            price: None,
            price_source: None,
            reasoning: shaped::<Capabilities>(item.capabilities).and_then(Capabilities::reasoning),
            images: None,
        }
    }
}

/// Anthropic's `capabilities` object: the effort levels and the thinking types
/// a model supports.
#[derive(Deserialize)]
struct Capabilities {
    #[serde(default)]
    effort: Option<Efforts>,
    #[serde(default)]
    thinking: Option<Thinking>,
}

impl Capabilities {
    fn reasoning(self) -> Option<Reasoning> {
        let mut reasoning = Reasoning::default();
        if let Some(efforts) = self.effort {
            for (level, support) in efforts.levels() {
                if support.supported {
                    reasoning.levels.insert(level);
                }
            }
        }
        if let Some(disabled) = self
            .thinking
            .and_then(|thinking| thinking.types)
            .and_then(|types| types.disabled)
        {
            reasoning.off = Some(disabled.supported);
        }
        (!reasoning.is_empty()).then_some(reasoning)
    }
}

/// The levels of `capabilities.effort`, each with a `supported` flag.
#[derive(Deserialize)]
struct Efforts {
    #[serde(default)]
    minimal: Option<Support>,
    #[serde(default)]
    low: Option<Support>,
    #[serde(default)]
    medium: Option<Support>,
    #[serde(default)]
    high: Option<Support>,
    #[serde(default)]
    xhigh: Option<Support>,
    #[serde(default)]
    max: Option<Support>,
}

impl Efforts {
    /// The named levels with their flags, lowest first.
    fn levels(self) -> impl Iterator<Item = (Level, Support)> {
        [
            (Level::Minimal, self.minimal),
            (Level::Low, self.low),
            (Level::Medium, self.medium),
            (Level::High, self.high),
            (Level::Xhigh, self.xhigh),
            (Level::Max, self.max),
        ]
        .into_iter()
        .filter_map(|(level, support)| Some((level, support?)))
    }
}

/// Whether one capability is supported.
#[derive(Deserialize)]
struct Support {
    #[serde(default)]
    supported: bool,
}

/// The `thinking` object of a model list.
#[derive(Deserialize)]
struct Thinking {
    #[serde(default)]
    types: Option<ThinkingTypes>,
}

/// The thinking types a model supports.
#[derive(Deserialize)]
struct ThinkingTypes {
    #[serde(default)]
    disabled: Option<Support>,
}

/// Reads Anthropic's own model list.
pub fn read_list(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    crate::catalog::data::<Item<'_>>(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_an_odd_capabilities_shape_is_no_data_not_a_failed_list() {
        let body = br#"{"data":[{"id":"claude-a","capabilities":["tool_use"]}]}"#;
        let entries = read_list(body).expect("the list still parses");
        assert_eq!(entries[0].id, "claude-a");
        assert_eq!(entries[0].reasoning, None);
    }

    #[test]
    fn list_reads_capabilities_effort_and_thinking() {
        let body = br#"{"data":[{"id":"claude","capabilities":{
            "effort":{"low":{"supported":true},"medium":{"supported":true},
                      "high":{"supported":true},"xhigh":{"supported":false},
                      "max":{"supported":false}},
            "thinking":{"types":{"disabled":{"supported":true}}}
        }}]}"#;
        let entries = read_list(body).expect("parses");
        let reasoning = entries[0].reasoning.as_ref().expect("a plan");
        assert_eq!(
            reasoning.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::Medium, Level::High]
        );
        assert_eq!(reasoning.off, Some(true));
        assert!(!reasoning.mandatory);
        assert_eq!(reasoning.default, None, "the list names no default level");
        // Thinking a model cannot disable is off false, not unknown.
        let always = br#"{"data":[{"id":"claude","capabilities":
            {"thinking":{"types":{"disabled":{"supported":false}}}}
        }]}"#;
        let entries = read_list(always).expect("parses");
        assert_eq!(
            entries[0].reasoning.as_ref().expect("a plan").off,
            Some(false)
        );
    }
}
