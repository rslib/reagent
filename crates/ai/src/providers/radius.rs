//! The radius preset.

use serde::Deserialize;

use super::{Auth, Models, Preset, Route, Wire};
use crate::catalog::{Entry, Price, PriceSource, sane};

pub const PRESET: Preset = Preset {
    name: "radius",
    routes: &[Route {
        name: "messages",
        models: Models::Any,
        wire: Wire::PiMessages,
        base_url: "https://radius.pi.dev/v1",
        base_url_env: Some("RADIUS_BASE_URL"),
        auth: Auth::Bearer,
        ..Route::DEFAULT
    }],
    api_key_env: "RADIUS_API_KEY",
    models_path: "/config",
    // The endpoint's own `/config` names every model and its price.
    models_dev: super::ModelsDev {
        id: None,
        prices: false,
    },
    read_list,
    // The endpoint lists its models without a key.
    public_list: true,
    ..Preset::DEFAULT
};

/// The endpoint's `/config` shape: a list of models under `models`.
#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    models: Vec<Item>,
}

/// One model of the `/config` shape.
#[derive(Deserialize)]
struct Item {
    id: Option<String>,
    #[serde(rename = "contextWindow")]
    context_window: Option<u64>,
    #[serde(rename = "maxTokens")]
    max_tokens: Option<u64>,
    cost: Option<ConfigCost>,
    enabled: Option<bool>,
}

impl Item {
    /// The entry this model lists, or `None` for a model the endpoint switched
    /// off.
    fn entry(self) -> Option<Entry> {
        let id = self.id?;
        if self.enabled == Some(false) {
            return None;
        }
        let price = self.cost.and_then(ConfigCost::price);
        Some(Entry {
            id,
            context_window: self.context_window,
            max_output: self.max_tokens,
            price_source: price.map(|_| PriceSource::Provider),
            price,
            reasoning: None,
            images: None,
        })
    }
}

/// The `cost` object of the `/config` shape, in USD per million tokens.
#[derive(Deserialize)]
struct ConfigCost {
    input: Option<f64>,
    output: Option<f64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<f64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<f64>,
}

impl ConfigCost {
    /// The price per token, or `None` when the list leaves out a token price.
    /// A cache price that is missing counts as free, as in a `pricing` object.
    fn price(self) -> Option<Price> {
        let per_million = 1_000_000.0;
        sane(Price {
            input: self.input? / per_million,
            output: self.output? / per_million,
            cache_read: self.cache_read.unwrap_or(0.0) / per_million,
            cache_write: self.cache_write.unwrap_or(0.0) / per_million,
        })
    }
}

/// Reads the endpoint's own `/config` list.
pub fn read_list(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    let listing: Listing = serde_json::from_slice(body)?;
    Ok(listing.models.into_iter().filter_map(Item::entry).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_config_lists_limits_and_prices() {
        let body = br#"{"baseUrl":"https://radius.pi.dev/v1","models":[
            {"id":"balanced","name":"Balanced","contextWindow":200000,"maxTokens":64000,
             "cost":{"input":1.0,"output":4.0,"cacheRead":0.1,"cacheWrite":1.0},"enabled":true},
            {"id":"small","contextWindow":32000,"maxTokens":8000,"enabled":true},
            {"id":"retired","contextWindow":8000,"maxTokens":1000,"enabled":false}
        ]}"#;
        let entries = read_list(body).expect("the config shape parses");
        assert_eq!(
            entries.iter().map(|entry| &*entry.id).collect::<Vec<_>>(),
            ["balanced", "small"],
            "a model the endpoint switched off is not listed"
        );
        assert_eq!(entries[0].context_window, Some(200_000));
        assert_eq!(entries[0].max_output, Some(64_000));
        let price = entries[0]
            .price
            .expect("the config prices the model per million");
        assert!((price.input - 0.000_001).abs() < 1e-12, "{price:?}");
        assert!((price.output - 0.000_004).abs() < 1e-12, "{price:?}");
        assert!((price.cache_read - 0.000_000_1).abs() < 1e-12, "{price:?}");
        assert_eq!(entries[0].price_source, Some(PriceSource::Provider));
        assert_eq!(entries[1].context_window, Some(32_000));
        assert_eq!(entries[1].price, None, "a model with no cost has no price");
    }
}
