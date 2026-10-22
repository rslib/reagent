//! The openai preset.

use super::{Compat, MaxTokensField, Preset, Wire};

pub const PRESET: Preset = Preset {
    name: "openai",
    wire: Wire::OpenAiResponses,
    base_url: "https://api.openai.com/v1",
    base_url_env: Some("OPENAI_BASE_URL"),
    api_key_env: "OPENAI_API_KEY",
    compat: Compat {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        reasoning_off: Some("none"),
        ..Compat::DEFAULT
    },
    ..Preset::DEFAULT
};
