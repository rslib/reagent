//! The chatgpt preset: a saved Sign in with ChatGPT login, not a key.

use super::{Compat, MaxTokensField, ModelsDev, Preset, Wire};

pub const PRESET: Preset = Preset {
    name: "chatgpt",
    wire: Wire::OpenAiResponses,
    base_url: "https://api.openai.com/v1",
    base_url_env: None,
    // No environment variable holds its key: its credential is a saved login
    // only, so the CLI asks for a login instead of naming a variable.
    api_key_env: "",
    models_path: "/models",
    compat: Compat {
        max_tokens_field: MaxTokensField::Omit,
        reasoning_off: Some("none"),
        ..Compat::DEFAULT
    },
    // A plan bills no tokens, so the catalogue fills the limits only.
    models_dev: ModelsDev {
        id: Some("openai"),
        prices: false,
    },
    peak: None,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{self, ReasoningField};

    #[test]
    fn chatgpt_preset_is_in_every_preset_with_its_own_fields() {
        let preset = providers::get("chatgpt").expect("the chatgpt preset");
        assert!(providers::ALL.iter().any(|one| one.name == PRESET.name));
        assert_eq!(preset.name, PRESET.name);
        assert_eq!(preset.wire, Wire::OpenAiResponses);
        assert_eq!(preset.base_url, "https://api.openai.com/v1");
        assert_eq!(preset.base_url_env, None);
        assert_eq!(preset.api_key_env, "");
        assert_eq!(preset.models_path, "/models");
        assert_eq!(
            preset.compat,
            Compat {
                max_tokens_field: MaxTokensField::Omit,
                reasoning_field: ReasoningField::Effort,
                reasoning_off: Some("none"),
                session_header: None,
            }
        );
    }
}
