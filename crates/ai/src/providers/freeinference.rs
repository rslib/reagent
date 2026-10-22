//! The freeinference preset.

use super::Preset;

pub const PRESET: Preset = Preset {
    name: "freeinference",
    base_url: "https://freeinference.org/v1",
    api_key_env: "FREEINFERENCE_API_KEY",
    ..Preset::DEFAULT
};
