//! The opencode-go preset.

use super::{Compat, Preset, Route};

pub const PRESET: Preset = Preset {
    name: "opencode-go",
    routes: &[Route {
        base_url: "https://opencode.ai/zen/go/v1",
        compat: Compat {
            session_header: Some("x-opencode-session"),
            ..Compat::DEFAULT
        },
        ..Route::DEFAULT
    }],
    api_key_env: "OPENCODE_API_KEY",
    ..Preset::DEFAULT
};
