//! The commandcode preset.

use super::{Compat, Preset};

pub const PRESET: Preset = Preset {
    name: "commandcode",
    base_url: "https://api.commandcode.ai/provider/v1",
    api_key_env: "COMMANDCODE_API_KEY",
    compat: Compat {
        reasoning_off: Some("off"),
        ..Compat::DEFAULT
    },
    ..Preset::DEFAULT
};
