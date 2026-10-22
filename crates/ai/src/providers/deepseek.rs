//! The deepseek preset.

use super::{Compat, Peak, Preset};

pub const PRESET: Preset = Preset {
    name: "deepseek",
    base_url: "https://api.deepseek.com",
    api_key_env: "DEEPSEEK_API_KEY",
    compat: Compat {
        reasoning_off: Some("none"),
        ..Compat::DEFAULT
    },
    // The catalogue's discount is a time of day: its price doubles in the peak
    // hours of a weekday. Chinese public holidays are off-peak and not modeled,
    // so a run on a holiday shows the peak price.
    peak: Some(Peak {
        windows: &[(1, 4), (6, 10)],
        weekdays_only: true,
        factor: 2.0,
    }),
    ..Preset::DEFAULT
};
