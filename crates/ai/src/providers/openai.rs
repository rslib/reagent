//! The openai preset.

use super::{Compat, MaxTokensField, Preset, Route, Wire};

/// The same endpoint serves Chat Completions too, which a named wire picks.
const CHAT: Route = Route {
    base_url: "https://api.openai.com/v1",
    base_url_env: Some("OPENAI_BASE_URL"),
    compat: Compat {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        reasoning_off: Some("none"),
        ..Compat::DEFAULT
    },
    ..Route::DEFAULT
};

pub const PRESET: Preset = Preset {
    name: "openai",
    routes: &[
        Route {
            name: "responses",
            wire: Wire::OpenAiResponses,
            ..CHAT
        },
        CHAT,
    ],
    api_key_env: "OPENAI_API_KEY",
    ..Preset::DEFAULT
};
