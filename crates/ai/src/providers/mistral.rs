//! The mistral preset.

use serde::Deserialize;

use super::{Compat, Preset, Route};
use crate::catalog::Entry;
use crate::levels::Reasoning;

/// The endpoint: the chat wire, which takes `reasoning_effort` and refuses
/// `stream_options`.
pub const ROUTE: Route = Route {
    base_url: "https://api.mistral.ai/v1",
    compat: Compat {
        reasoning_off: Some("none"),
        stream_usage: false,
        replay_reasoning: false,
        ..Compat::DEFAULT
    },
    ..Route::DEFAULT
};

pub const PRESET: Preset = Preset {
    name: "mistral",
    routes: &[ROUTE],
    api_key_env: "MISTRAL_API_KEY",
    read_list,
    ..Preset::DEFAULT
};

/// Mistral's model list, of which only the chatting, unarchived models count.
#[derive(Deserialize)]
struct List {
    #[serde(default)]
    data: Vec<Item>,
}

/// One entry of Mistral's own model list.
#[derive(Deserialize)]
struct Item {
    id: String,
    #[serde(default)]
    capabilities: Option<Capabilities>,
    /// Mistral's name for the context window.
    max_context_length: Option<u64>,
    #[serde(default)]
    archived: Option<bool>,
}

/// The tasks a model takes. A model that does not chat takes no request.
#[derive(Default, Deserialize)]
struct Capabilities {
    #[serde(default)]
    completion_chat: bool,
    /// Whether the model thinks, when the list says.
    #[serde(default)]
    reasoning: Option<bool>,
}

impl Item {
    /// The entry this model lists, or `None` for a model that does not chat or
    /// that the provider archived.
    fn entry(self) -> Option<Entry> {
        let capabilities = self.capabilities.unwrap_or_default();
        let chat = capabilities.completion_chat && self.archived != Some(true);
        // Only a plain `false` marks a model that does not think; the
        // catalogue fills the levels of one that does.
        let reasoning = (capabilities.reasoning == Some(false)).then(Reasoning::never);
        chat.then_some(Entry {
            id: self.id,
            context_window: self.max_context_length,
            max_output: None,
            price: None,
            price_source: None,
            reasoning,
            images: None,
        })
    }
}

/// Reads Mistral's own model list, keeping only the models that chat.
pub fn read_list(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    let list: List = serde_json::from_slice(body)?;
    Ok(list.data.into_iter().filter_map(Item::entry).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers;

    #[test]
    fn mistral_preset_is_in_every_preset() {
        let preset = providers::get("mistral").expect("the mistral preset");
        assert!(providers::ALL.iter().any(|one| one.name == PRESET.name));
        assert_eq!(preset.default_route().base_url, ROUTE.base_url);
        assert_eq!(preset.models_path, "/models");
    }

    #[test]
    fn mistral_list_keeps_the_models_that_chat_only() {
        let body = br#"{"object":"list","data":[
            {"id":"mistral-large-latest","capabilities":{"completion_chat":true,"function_calling":true,"vision":true},"max_context_length":131072,"aliases":["mistral-large"],"archived":false},
            {"id":"mistral-old","capabilities":{"completion_chat":true},"max_context_length":32768,"archived":true},
            {"id":"mistral-embed","capabilities":{"completion_chat":false},"max_context_length":8192}
        ]}"#;
        let entries = read_list(body).expect("the list parses");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].id, "mistral-large-latest");
        assert_eq!(entries[0].context_window, Some(131_072));
        assert_eq!(entries[0].price, None, "the catalogue fills the price");
        assert_eq!(entries[0].images, None);
    }

    /// Mistral's `capabilities.reasoning` says whether a model thinks; only a
    /// plain `false` marks it, so a list without the field changes nothing.
    #[test]
    fn mistral_list_marks_the_models_that_do_not_think() {
        let body = br#"{"data":[
            {"id":"ministral-8b-latest","capabilities":{"completion_chat":true,"reasoning":false}},
            {"id":"magistral-medium-latest","capabilities":{"completion_chat":true,"reasoning":true}},
            {"id":"older","capabilities":{"completion_chat":true}}
        ]}"#;
        let entries = read_list(body).expect("the list parses");
        assert_eq!(entries[0].reasoning, Some(Reasoning::never()));
        assert_eq!(entries[1].reasoning, None, "the catalogue fills its levels");
        assert_eq!(entries[2].reasoning, None);
    }

    #[test]
    fn mistral_list_of_an_odd_shape_is_no_model_not_a_failed_list() {
        let body = br#"{"object":"list","data":[{"id":"m"},{"id":"n","capabilities":null}]}"#;
        let entries = read_list(body).expect("the list still parses");
        assert!(entries.is_empty(), "{entries:?}");
        assert!(read_list(b"{}").expect("no data is no model").is_empty());
    }
}
