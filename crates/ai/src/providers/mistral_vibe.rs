//! The mistral-vibe preset: the Mistral endpoint with the key of a Vibe plan
//! login, not a key variable.

use super::mistral;
use super::{ModelsDev, Preset};

pub const PRESET: Preset = Preset {
    name: "mistral-vibe",
    routes: &[mistral::ROUTE],
    // No environment variable holds its key: its credential is the key
    // `reagent login mistral-vibe` saves, so the CLI asks for a login instead of
    // naming a variable.
    api_key_env: "",
    // The endpoint lists the same models as the plain preset, so the catalogue
    // fills the limits from the same provider. A Vibe plan bills no tokens, so
    // it fills no price.
    models_dev: ModelsDev {
        id: Some("mistral"),
        prices: false,
    },
    read_list: mistral::read_list,
    ..Preset::DEFAULT
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers;

    #[test]
    fn mistral_vibe_preset_shares_the_endpoint_and_the_list() {
        let preset = providers::get("mistral-vibe").expect("the mistral-vibe preset");
        assert!(providers::ALL.iter().any(|one| one.name == PRESET.name));
        assert_eq!(preset.default_route().base_url, "https://api.mistral.ai/v1");
        assert_eq!(
            preset.default_route().compat,
            providers::get("mistral")
                .expect("the mistral preset")
                .default_route()
                .compat,
            "the same wire, the same flags"
        );
        assert_eq!(preset.api_key_env, "", "a saved login holds the key");
        assert_eq!(preset.models_dev.id, Some("mistral"));
        assert!(!preset.models_dev.prices, "a plan bills no tokens");
        assert!(
            std::ptr::fn_addr_eq(
                preset.read_list,
                mistral::read_list as crate::catalog::ReadList
            ),
            "the same model list shape"
        );
    }
}
