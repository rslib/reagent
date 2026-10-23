//! The synthetic preset.

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{Compat, Preset, Route, Wire};
use crate::catalog::{Entry, Price, PriceSource, sane, shaped};
use crate::levels::{Reasoning, Words};

pub const PRESET: Preset = Preset {
    name: "synthetic",
    // One key serves both the usage plan and the subscription. The Messages
    // endpoint serves the same models; a named wire picks it.
    routes: &[
        Route {
            base_url: "https://api.synthetic.new/openai/v1",
            compat: Compat {
                reasoning_off: Some("none"),
                ..Compat::DEFAULT
            },
            ..Route::DEFAULT
        },
        Route {
            name: "messages",
            wire: Wire::Anthropic,
            base_url: "https://api.synthetic.new/anthropic",
            ..Route::DEFAULT
        },
    ],
    api_key_env: "SYNTHETIC_API_KEY",
    read_list,
    ..Preset::DEFAULT
};

/// One entry of Synthetic's own model list. Its prices carry a leading `$` and
/// its reasoning lives in a `reasoning_parameters` object.
#[derive(Deserialize)]
struct Item<'a> {
    id: String,
    context_length: Option<u64>,
    /// Synthetic's own name for the cap on reply tokens.
    max_output_length: Option<u64>,
    pricing: Option<Pricing>,
    /// The `reasoning_parameters` object, kept raw until the entry is built, so
    /// one odd model does not fail the whole list.
    #[serde(default, borrow)]
    reasoning_parameters: Option<&'a RawValue>,
}

impl<'a> From<Item<'a>> for Entry {
    fn from(item: Item<'a>) -> Entry {
        let price = item.pricing.and_then(Pricing::price);
        Entry {
            id: item.id,
            context_window: item.context_length,
            max_output: item.max_output_length,
            price_source: price.map(|_| PriceSource::Provider),
            price,
            reasoning: shaped::<SyntheticEfforts>(item.reasoning_parameters)
                .and_then(SyntheticEfforts::reasoning),
        }
    }
}

/// Synthetic's `pricing` object. Every price is a string with a leading `$`,
/// in USD per token.
#[derive(Deserialize)]
struct Pricing {
    #[serde(default, deserialize_with = "dollars")]
    prompt: Option<f64>,
    #[serde(default, deserialize_with = "dollars")]
    completion: Option<f64>,
    #[serde(default, deserialize_with = "dollars")]
    input_cache_reads: Option<f64>,
    #[serde(default, deserialize_with = "dollars")]
    input_cache_writes: Option<f64>,
}

impl Pricing {
    /// The price, or `None` when the listing leaves out a token price or writes
    /// one that is not a number. A cache price that is missing or bad counts as
    /// free, which is the common case for a provider that does not say.
    fn price(self) -> Option<Price> {
        sane(Price {
            input: self.prompt?,
            output: self.completion?,
            cache_read: self.input_cache_reads.unwrap_or(0.0),
            cache_write: self.input_cache_writes.unwrap_or(0.0),
        })
    }
}

/// Reads one price, written as a string with a leading `$`, or as a number.
/// Anything else, and text that is not a number, is `None`.
fn dollars<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Dollars;

    impl serde::de::Visitor<'_> for Dollars {
        type Value = Option<f64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a price as a number or a string with a leading dollar sign")
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
            Ok(Some(value))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
            Ok(Some(value as f64))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
            Ok(Some(value as f64))
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            let value = value.trim();
            Ok(value.strip_prefix('$').unwrap_or(value).parse().ok())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(Dollars)
}

/// Synthetic's `reasoning_parameters` object: every effort the model takes, so
/// a list without `none` means thinking cannot be turned off.
#[derive(Deserialize)]
struct SyntheticEfforts {
    #[serde(default)]
    efforts: Words,
}

impl SyntheticEfforts {
    fn reasoning(self) -> Option<Reasoning> {
        let mut reasoning = self.efforts.0;
        if reasoning.is_empty() {
            return None;
        }
        reasoning.off.get_or_insert(false);
        Some(reasoning)
    }
}

/// Reads Synthetic's own model list.
pub fn read_list(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    crate::catalog::data::<Item<'_>>(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::levels::Level;

    #[test]
    fn list_an_odd_reasoning_parameters_shape_is_no_data_not_a_failed_list() {
        let body = br#"{"data":[{"id":"a","reasoning_parameters":"x"},{"id":"b","reasoning_parameters":{"efforts":[1]}}]}"#;
        let entries = read_list(body).expect("the list still parses");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.reasoning.is_none()));
    }

    #[test]
    fn list_plural_cache_names_are_read() {
        let body = br#"{"data":[{"id":"q","pricing":{"prompt":"0.08","completion":"0.28","input_cache_reads":"0.01","input_cache_writes":"0.02"}}]}"#;
        let price = read_list(body).expect("parses")[0].price.expect("a price");
        assert_eq!(price.cache_read, 0.01);
        assert_eq!(price.cache_write, 0.02);
    }

    #[test]
    fn list_unknown_efforts_are_kept() {
        let body = br#"{"data":[{"id":"m","reasoning_parameters":{"efforts":["low","ultra","high","Ultra"]}}]}"#;
        let entries = read_list(body).expect("parses");
        let reasoning = entries[0].reasoning.as_ref().expect("a plan");
        assert_eq!(
            reasoning.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::High]
        );
        assert_eq!(reasoning.unknown, ["ultra"]);
    }

    #[test]
    fn list_reads_dollar_prices_output_limit_and_efforts() {
        let body = br#"{"data":[{"id":"hf:zai-org/GLM-5.3","context_length":524288,
            "max_output_length":65536,
            "reasoning_parameters":{"efforts":["none","low","high"]},
            "pricing":{"prompt":"$0.0000006","completion":"$0.0000012","image":"0",
                "request":"0","input_cache_reads":"$0.00000003","input_cache_writes":"0"}}]}"#;
        let entry = &read_list(body).expect("parses")[0];
        assert_eq!(entry.id, "hf:zai-org/GLM-5.3");
        assert_eq!(entry.max_output, Some(65536));
        let price = entry.price.expect("a price");
        assert_eq!(price.input, 0.0000006);
        assert_eq!(price.output, 0.0000012);
        assert_eq!(price.cache_read, 0.00000003);
        let reasoning = entry.reasoning.as_ref().expect("a plan");
        assert_eq!(
            reasoning.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::High]
        );
        assert_eq!(reasoning.off, Some(true));
    }

    #[test]
    fn list_without_none_cannot_turn_thinking_off() {
        let body = br#"{"data":[{"id":"hf:zai-org/GLM-5.3",
            "reasoning_parameters":{"efforts":["low","high","max"]}}]}"#;
        let entries = read_list(body).expect("parses");
        let reasoning = entries[0].reasoning.as_ref().expect("a plan");
        assert_eq!(reasoning.off, Some(false));
    }

    #[test]
    fn list_a_bare_dollar_is_no_price() {
        let body = br#"{"data":[{"id":"m","pricing":{"prompt":"$","completion":"$1"}}]}"#;
        assert_eq!(read_list(body).expect("parses")[0].price, None);
    }
}
