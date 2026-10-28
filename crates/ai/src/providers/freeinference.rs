//! The freeinference preset.

use serde::Deserialize;

use super::{Preset, Route};
use crate::catalog::{Entry, Price, PriceSource, sane};

pub const PRESET: Preset = Preset {
    name: "freeinference",
    routes: &[Route {
        base_url: "https://freeinference.org/v1",
        ..Route::DEFAULT
    }],
    api_key_env: "FREEINFERENCE_API_KEY",
    // Its `pricing` uses OpenRouter's field names in USD per million tokens,
    // which its own reader divides down to a per-token price.
    read_list,
    ..Preset::DEFAULT
};

/// One entry of FreeInference's own model list.
#[derive(Deserialize)]
struct Item {
    id: String,
    context_length: Option<u64>,
    /// FreeInference's own name for the cap on reply tokens.
    max_output_length: Option<u64>,
    pricing: Option<Pricing>,
}

impl From<Item> for Entry {
    fn from(item: Item) -> Entry {
        let price = item.pricing.and_then(Pricing::price);
        Entry {
            id: item.id,
            context_window: item.context_length,
            max_output: item.max_output_length,
            price_source: price.map(|_| PriceSource::Provider),
            price,
            reasoning: None,
            images: None,
        }
    }
}

/// FreeInference's `pricing` object: USD per million tokens, under OpenRouter's
/// plural cache names.
#[derive(Deserialize)]
struct Pricing {
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    prompt: Option<f64>,
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    completion: Option<f64>,
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    input_cache_reads: Option<f64>,
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    input_cache_writes: Option<f64>,
}

impl Pricing {
    /// The price per token, from a listing in USD per million tokens.
    fn price(self) -> Option<Price> {
        const MILLION: f64 = 1_000_000.0;
        sane(Price {
            input: self.prompt? / MILLION,
            output: self.completion? / MILLION,
            cache_read: self.input_cache_reads.unwrap_or(0.0) / MILLION,
            cache_write: self.input_cache_writes.unwrap_or(0.0) / MILLION,
        })
    }
}

/// Reads FreeInference's own model list, whose prices are per million tokens.
pub fn read_list(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    crate::catalog::data::<Item>(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_scales_per_million_and_openrouter_stays() {
        // FreeInference writes its prices in USD per million tokens, so its own
        // reader divides by a million.
        let body = br#"{"data":[{"id":"q","pricing":{"prompt":"0.08","completion":"0.28","input_cache_reads":"0.01","input_cache_writes":"0.02"}}]}"#;
        let price = read_list(body).expect("parses")[0].price.expect("a price");
        assert!(
            (price.input - 0.000_000_08).abs() < 1e-15,
            "{}",
            price.input
        );
        assert!(
            (price.output - 0.000_000_28).abs() < 1e-15,
            "{}",
            price.output
        );
        assert!(
            (price.cache_read - 0.000_000_01).abs() < 1e-15,
            "{}",
            price.cache_read
        );
        assert!(
            (price.cache_write - 0.000_000_02).abs() < 1e-15,
            "{}",
            price.cache_write
        );

        // The OpenRouter-shape reader takes the same names in USD per token and
        // leaves them alone.
        let router = crate::catalog::list::read(
            br#"{"data":[{"id":"q","pricing":{"prompt":"0.08","completion":"0.28","input_cache_read":"0.01","input_cache_write":"0"}}]}"#,
        )
        .expect("parses");
        assert_eq!(
            router[0].price,
            Some(Price {
                input: 0.08,
                output: 0.28,
                cache_read: 0.01,
                cache_write: 0.0,
            })
        );
    }
}
