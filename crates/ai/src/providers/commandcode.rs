//! The commandcode preset.

use super::{Compat, Models, Preset, Route, Wire};

/// The endpoint for every other model; it serves the Responses API too.
const CHAT: Route = Route {
    base_url: "https://api.commandcode.ai/provider/v1",
    compat: Compat {
        reasoning_off: Some("off"),
        ..Compat::DEFAULT
    },
    ..Route::DEFAULT
};

pub const PRESET: Preset = Preset {
    name: "commandcode",
    // Claude models answer on the Messages endpoint only; chat refuses them.
    routes: &[
        Route {
            name: "messages",
            models: Models::Prefix("claude-"),
            wire: Wire::Anthropic,
            base_url: "https://api.commandcode.ai/provider",
            ..Route::DEFAULT
        },
        CHAT,
        Route {
            name: "responses",
            wire: Wire::OpenAiResponses,
            ..CHAT
        },
    ],
    api_key_env: "COMMANDCODE_API_KEY",
    ..Preset::DEFAULT
};
