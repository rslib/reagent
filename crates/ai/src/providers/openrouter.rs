//! The openrouter preset.

use super::{Compat, Preset, ReasoningField};

pub const PRESET: Preset = Preset {
    name: "openrouter",
    base_url: "https://openrouter.ai/api/v1",
    api_key_env: "OPENROUTER_API_KEY",
    compat: Compat {
        reasoning_field: ReasoningField::EffortObject,
        reasoning_off: Some("none"),
        ..Compat::DEFAULT
    },
    ..Preset::DEFAULT
};
