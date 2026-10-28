//! The chatgpt preset: a saved Sign in with ChatGPT login, not a key.

use serde::Deserialize;

use super::{Compat, MaxTokensField, ModelsDev, Preset, Route, Wire};
use crate::catalog::Entry;

pub const PRESET: Preset = Preset {
    name: "chatgpt",
    routes: &[Route {
        name: "responses",
        wire: Wire::OpenAiResponses,
        base_url: "https://api.openai.com/v1",
        compat: Compat {
            max_tokens_field: MaxTokensField::Omit,
            reasoning_off: Some("none"),
            ..Compat::DEFAULT
        },
        ..Route::DEFAULT
    }],
    // No environment variable holds its key: its credential is a saved login
    // only, so the CLI asks for a login instead of naming a variable.
    api_key_env: "",
    models_path: "/models",
    // A plan bills no tokens, so the catalogue fills the limits only.
    models_dev: ModelsDev {
        id: Some("openai"),
        prices: false,
    },
    peak: None,
    read_list,
    ..Preset::DEFAULT
};

/// The ChatGPT plan shape: a list of plans, each with a slug and a visibility.
#[derive(Deserialize)]
struct Plan {
    #[serde(default)]
    models: Vec<Item>,
}

/// One plan of a ChatGPT model list.
#[derive(Deserialize)]
struct Item {
    slug: Option<String>,
    visibility: Option<String>,
}

impl Item {
    /// The entry this plan lists, or `None` for a plan the provider hides.
    fn entry(self) -> Option<Entry> {
        let slug = self.slug?;
        (self.visibility.as_deref() == Some("list")).then_some(Entry {
            id: slug,
            context_window: None,
            max_output: None,
            price: None,
            price_source: None,
            reasoning: None,
            images: None,
        })
    }
}

/// Reads the ChatGPT plan shape, keeping only the listed plans.
pub fn read_list(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    let plan: Plan = serde_json::from_slice(body)?;
    Ok(plan.models.into_iter().filter_map(Item::entry).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{self, ReasoningField};

    #[test]
    fn chatgpt_preset_is_in_every_preset_with_its_own_fields() {
        let preset = providers::get("chatgpt").expect("the chatgpt preset");
        assert!(providers::ALL.iter().any(|one| one.name == PRESET.name));
        assert_eq!(preset.name, PRESET.name);
        assert_eq!(preset.default_route().wire, Wire::OpenAiResponses);
        assert_eq!(preset.default_route().base_url, "https://api.openai.com/v1");
        assert_eq!(preset.default_route().base_url_env, None);
        assert_eq!(preset.api_key_env, "");
        assert_eq!(preset.models_path, "/models");
        assert_eq!(
            preset.default_route().compat,
            Compat {
                max_tokens_field: MaxTokensField::Omit,
                reasoning_field: ReasoningField::Effort,
                reasoning_off: Some("none"),
                session_header: None,
            }
        );
    }

    #[test]
    fn list_keeps_the_listed_slugs() {
        let body = br#"{"models":[{"slug":"gpt-5","visibility":"list"},{"slug":"gpt-5-internal","visibility":"hidden"}]}"#;
        let entries = read_list(body).expect("the plan shape parses");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].id, "gpt-5");
        assert_eq!(entries[0].context_window, None);
        assert_eq!(entries[0].price, None);
    }
}
