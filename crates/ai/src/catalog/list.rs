//! The OpenRouter model-list shape, which several gateways copy.
//!
//! A listing is `{"data":[{"id", "context_length", "top_provider", "pricing",
//! "reasoning"}]}`, with each price a string or a number and `reasoning`
//! naming the efforts the model takes.

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::catalog::{self, Entry, Price, PriceSource};
use crate::levels::{Level, Reasoning, Word, Words};

/// One entry of an OpenRouter-shape list.
#[derive(Deserialize)]
pub(crate) struct Item<'a> {
    id: String,
    context_length: Option<u64>,
    top_provider: Option<TopProvider>,
    pricing: Option<Pricing>,
    /// The `reasoning` object, kept raw until the entry is built, so one odd
    /// model does not fail the whole list.
    #[serde(default, borrow)]
    reasoning: Option<&'a RawValue>,
}

impl Item<'_> {
    /// The reasoning plan this model writes, or `None` when the object names no
    /// level and no `none`.
    fn reasoning(&self) -> Option<Reasoning> {
        catalog::shaped::<RouterReasoning<'_>>(self.reasoning).and_then(RouterReasoning::reasoning)
    }
}

impl<'a> From<Item<'a>> for Entry {
    fn from(item: Item<'a>) -> Entry {
        let reasoning = item.reasoning();
        let price = item.pricing.and_then(Pricing::price);
        Entry {
            id: item.id,
            context_window: item.context_length,
            max_output: item.top_provider.and_then(|top| top.max_completion_tokens),
            price_source: price.map(|_| PriceSource::Provider),
            price,
            reasoning,
        }
    }
}

/// The `top_provider` object of one listed model.
#[derive(Deserialize)]
pub(crate) struct TopProvider {
    /// The cap on reply tokens.
    pub(crate) max_completion_tokens: Option<u64>,
}

/// The `pricing` object of one listed model. OpenRouter writes each price as a
/// string, so a listing is read as either text or a number.
#[derive(Deserialize)]
pub(crate) struct Pricing {
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    pub(crate) prompt: Option<f64>,
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    pub(crate) completion: Option<f64>,
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    pub(crate) input_cache_read: Option<f64>,
    #[serde(default, deserialize_with = "crate::catalog::amount")]
    pub(crate) input_cache_write: Option<f64>,
}

impl Pricing {
    /// The price, or `None` when the listing leaves out a token price or writes
    /// one that is not a number. A cache price that is missing or bad counts as
    /// free, which is the common case for a provider that does not say.
    pub(crate) fn price(self) -> Option<Price> {
        catalog::sane(Price {
            input: self.prompt?,
            output: self.completion?,
            cache_read: self.input_cache_read.unwrap_or(0.0),
            cache_write: self.input_cache_write.unwrap_or(0.0),
        })
    }
}

/// OpenRouter's `reasoning` object: whether thinking is mandatory and which
/// efforts the model takes.
#[derive(Deserialize)]
struct RouterReasoning<'a> {
    #[serde(default)]
    mandatory: bool,
    #[serde(default)]
    supported_efforts: Words,
    #[serde(default, borrow)]
    default_effort: Option<Word<'a>>,
}

impl RouterReasoning<'_> {
    /// The plan these efforts describe, or `None` when they name no level and
    /// no `none`.
    fn reasoning(self) -> Option<Reasoning> {
        let mut reasoning = self.supported_efforts.0;
        reasoning.mandatory = self.mandatory;
        reasoning.default = self
            .default_effort
            .as_ref()
            .map(Word::as_str)
            .and_then(Level::parse);
        (!reasoning.is_empty()).then_some(reasoning)
    }
}

/// Reads an OpenRouter-shape reply, which most gateways write.
pub fn read(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    catalog::data::<Item<'_>>(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_reads_the_limits_when_present() {
        let body = br#"{"data":[
            {"id":"a","context_length":128000,"top_provider":{"max_completion_tokens":4096},"x":1},
            {"id":"b"},
            {"id":"c","top_provider":{}}
        ]}"#;
        let entries = read(body).expect("parses");
        let entry = |id: &str, window, output| Entry {
            id: id.into(),
            context_window: window,
            max_output: output,
            price: None,
            price_source: None,
            reasoning: None,
        };
        assert_eq!(
            entries,
            [
                entry("a", Some(128_000), Some(4096)),
                entry("b", None, None),
                entry("c", None, None)
            ]
        );
    }

    #[test]
    fn list_an_odd_reasoning_shape_is_no_data_not_a_failed_list() {
        let body = br#"{"data":[{"id":"a","reasoning":true},{"id":"b","reasoning":{"supported_efforts":"x"}}]}"#;
        let entries = read(body).expect("the list still parses");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.reasoning.is_none()));
    }

    #[test]
    fn list_reads_the_plain_data_shape() {
        let old =
            read(br#"{"data":[{"id":"a","context_length":8000}]}"#).expect("the data shape parses");
        assert_eq!(old[0].id, "a");
        assert_eq!(old[0].context_window, Some(8000));
    }

    #[test]
    fn list_reads_the_pricing_object() {
        let body = br#"{"data":[
            {"id":"priced","pricing":{
                "prompt":"0.000001","completion":"0.000002",
                "input_cache_read":"0.0000001","input_cache_write":"0.0000012"
            }},
            {"id":"no-cache-prices","pricing":{"prompt":"0.000003","completion":"bad"}},
            {"id":"prompt-only","pricing":{"prompt":"0.000001"}},
            {"id":"empty","pricing":{}}
        ]}"#;
        let entries = read(body).expect("parses");
        assert_eq!(
            entries[0].price,
            Some(Price {
                input: 0.000_001,
                output: 0.000_002,
                cache_read: 0.000_000_1,
                cache_write: 0.000_001_2,
            })
        );
        // A cache price that is missing or unreadable is free, but a missing
        // token price leaves the model with no price at all.
        assert_eq!(entries[1].price, None, "a bad completion price is no price");
        assert_eq!(entries[2].price, None);
        assert_eq!(entries[3].price, None);

        let cache_prices = br#"{"data":[{"id":"m","pricing":{
            "prompt":"0.000001","completion":"0.000002","input_cache_read":"nope"
        }}]}"#;
        let entries = read(cache_prices).expect("parses");
        assert_eq!(
            entries[0].price,
            Some(Price {
                input: 0.000_001,
                output: 0.000_002,
                cache_read: 0.0,
                cache_write: 0.0,
            })
        );
    }

    #[test]
    fn list_ignores_a_negative_or_non_finite_price() {
        let body = br#"{"data":[
            {"id":"router","pricing":{"prompt":"-1","completion":"-1"}},
            {"id":"nan","pricing":{"prompt":"nan","completion":"0.000002"}},
            {"id":"inf","pricing":{"prompt":"0.000001","completion":"inf"}},
            {"id":"bad-cache","pricing":{"prompt":"0","completion":"0","input_cache_read":"-1"}}
        ]}"#;
        let entries = read(body).expect("parses");
        assert!(
            entries.iter().all(|entry| entry.price.is_none()),
            "{entries:?}"
        );
    }

    #[test]
    fn list_rejects_text_that_is_not_json() {
        assert!(read(b"nope").is_err());
    }

    #[test]
    fn list_reads_efforts_mandatory_and_none() {
        let body = br#"{"data":[{"id":"routed","reasoning":{
            "mandatory":true,"default_enabled":true,
            "supported_efforts":["none","low","medium","high"],"default_effort":"medium"
        }}]}"#;
        let entries = read(body).expect("parses");
        let reasoning = entries[0].reasoning.as_ref().expect("a plan");
        assert_eq!(
            reasoning.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::Medium, Level::High],
            "a none effort is not a level"
        );
        assert_eq!(
            reasoning.off,
            Some(true),
            "a none effort turns thinking off"
        );
        assert!(reasoning.mandatory);
        assert_eq!(reasoning.default, Some(Level::Medium));
        // A model whose object names no effort leaves no plan behind.
        let plain = read(br#"{"data":[{"id":"m","reasoning":{}}]}"#).expect("parses");
        assert_eq!(plain[0].reasoning, None);
    }
}
