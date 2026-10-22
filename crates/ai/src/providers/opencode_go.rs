//! The opencode-go preset.

use super::{Compat, Preset};

pub const PRESET: Preset = Preset {
    name: "opencode-go",
    base_url: "https://opencode.ai/zen/go/v1",
    api_key_env: "OPENCODE_API_KEY",
    compat: Compat {
        session_header: Some("x-opencode-session"),
        ..Compat::DEFAULT
    },
    ..Preset::DEFAULT
};
