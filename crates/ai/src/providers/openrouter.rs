//! The openrouter preset.

use super::{Auth, Compat, Models, Preset, ReasoningField, Route, Wire};

/// The endpoint for every model; it serves the Responses API too.
const CHAT: Route = Route {
    base_url: "https://openrouter.ai/api/v1",
    compat: Compat {
        reasoning_field: ReasoningField::EffortObject,
        reasoning_off: Some("none"),
        ..Compat::DEFAULT
    },
    ..Route::DEFAULT
};

pub const PRESET: Preset = Preset {
    name: "openrouter",
    // The Messages endpoint is for Anthropic models and takes a bearer key
    // only; a named wire picks it.
    routes: &[
        CHAT,
        Route {
            name: "responses",
            wire: Wire::OpenAiResponses,
            ..CHAT
        },
        Route {
            name: "messages",
            models: Models::Prefix("anthropic/"),
            wire: Wire::Anthropic,
            base_url: "https://openrouter.ai/api",
            auth: Auth::Bearer,
            ..Route::DEFAULT
        },
    ],
    api_key_env: "OPENROUTER_API_KEY",
    ..Preset::DEFAULT
};
