//! The anthropic preset.

use super::{Preset, Wire};

pub const PRESET: Preset = Preset {
    name: "anthropic",
    wire: Wire::Anthropic,
    base_url: "https://api.anthropic.com",
    base_url_env: Some("ANTHROPIC_BASE_URL"),
    api_key_env: "ANTHROPIC_API_KEY",
    ..Preset::DEFAULT
};
