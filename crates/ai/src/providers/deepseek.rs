//! The deepseek preset.

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{Compat, Peak, Preset, Route, Wire};
use crate::catalog::list::{Pricing, TopProvider};
use crate::catalog::{Entry, PriceSource, shaped};
use crate::levels::{Level, Reasoning, Word, Words};

pub const PRESET: Preset = Preset {
    name: "deepseek",
    // The Messages endpoint serves the same models; a named wire picks it.
    routes: &[
        Route {
            base_url: "https://api.deepseek.com",
            compat: Compat {
                reasoning_off: Some("none"),
                ..Compat::DEFAULT
            },
            ..Route::DEFAULT
        },
        Route {
            name: "messages",
            wire: Wire::Anthropic,
            base_url: "https://api.deepseek.com/anthropic",
            ..Route::DEFAULT
        },
    ],
    api_key_env: "DEEPSEEK_API_KEY",
    // The catalogue's discount is a time of day: its price doubles in the peak
    // hours of a weekday. Chinese public holidays are off-peak and not modeled,
    // so a run on a holiday shows the peak price.
    peak: Some(Peak {
        windows: &[(1, 4), (6, 10)],
        weekdays_only: true,
        factor: 2.0,
    }),
    read_list,
    ..Preset::DEFAULT
};

/// One entry of DeepSeek's own model list. It names the context window and the
/// reply cap its own way, and writes the reasoning levels in an `effort`
/// object.
#[derive(Deserialize)]
struct Item<'a> {
    id: String,
    context_length: Option<u64>,
    /// DeepSeek's own name for `context_length`.
    context_window: Option<u64>,
    top_provider: Option<TopProvider>,
    /// DeepSeek's own name for `top_provider.max_completion_tokens`.
    max_output_tokens: Option<u64>,
    pricing: Option<Pricing>,
    /// The `effort` object, kept raw until the entry is built, so one odd model
    /// does not fail the whole list.
    #[serde(default, borrow)]
    effort: Option<&'a RawValue>,
}

impl<'a> From<Item<'a>> for Entry {
    fn from(item: Item<'a>) -> Entry {
        let reasoning = shaped::<Effort<'_>>(item.effort).and_then(Effort::reasoning);
        // The list may write either name; the first present wins.
        let price = item.pricing.and_then(Pricing::price);
        Entry {
            id: item.id,
            context_window: item.context_window.or(item.context_length),
            max_output: item
                .max_output_tokens
                .or_else(|| item.top_provider.and_then(|top| top.max_completion_tokens)),
            price_source: price.map(|_| PriceSource::Provider),
            price,
            reasoning,
            images: None,
        }
    }
}

/// DeepSeek's `effort` object: the levels the model takes and the one it starts
/// at. It does not say whether thinking can be turned off.
#[derive(Deserialize)]
struct Effort<'a> {
    #[serde(default)]
    supported_levels: Words,
    #[serde(default, borrow)]
    default_level: Option<Word<'a>>,
}

impl Effort<'_> {
    fn reasoning(self) -> Option<Reasoning> {
        let mut reasoning = self.supported_levels.0;
        reasoning.default = self
            .default_level
            .as_ref()
            .map(Word::as_str)
            .and_then(Level::parse);
        (!reasoning.is_empty()).then_some(reasoning)
    }
}

/// Reads DeepSeek's own model list.
pub fn read_list(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    crate::catalog::data::<Item<'_>>(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Price;

    #[test]
    fn list_reads_the_deepseek_limit_names() {
        let body = br#"{"data":[{"id":"deepseek-flash","context_window":1048576,"max_output_tokens":393216}]}"#;
        let entries = read_list(body).expect("parses");
        assert_eq!(entries[0].id, "deepseek-flash");
        assert_eq!(entries[0].context_window, Some(1_048_576));
        assert_eq!(entries[0].max_output, Some(393_216));
        assert_eq!(entries[0].price, None);
        // The first present name wins, and a price a list writes is its own.
        let both = br#"{"data":[{
            "id":"m","context_window":10,"context_length":20,
            "max_output_tokens":3,"top_provider":{"max_completion_tokens":4},
            "pricing":{"prompt":"0.000001","completion":"0.000002"}
        }]}"#;
        let entries = read_list(both).expect("parses");
        assert_eq!(entries[0].context_window, Some(10));
        assert_eq!(entries[0].max_output, Some(3));
        assert_eq!(entries[0].price_source, Some(PriceSource::Provider));
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
    fn list_reads_effort_supported_levels_and_default() {
        let body = br#"{"data":[{"id":"deepseek-flash","effort":{
            "supported_levels":["low","medium","high","max"],"default_level":"high"
        }}]}"#;
        let entries = read_list(body).expect("parses");
        let reasoning = entries[0].reasoning.as_ref().expect("a reasoning plan");
        assert_eq!(
            reasoning.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::Medium, Level::High, Level::Max]
        );
        assert_eq!(reasoning.default, Some(Level::High));
        assert_eq!(reasoning.off, None, "deepseek does not say");
        assert!(!reasoning.mandatory);
    }

    #[test]
    fn list_effort_unknown_words_are_skipped() {
        let body = br#"{"data":[{"id":"m","effort":{
            "supported_levels":["low","bogus","HIGH","","max"],"default_level":"bogus"
        }}]}"#;
        let entries = read_list(body).expect("parses");
        let reasoning = entries[0].reasoning.as_ref().expect("a plan");
        assert_eq!(
            reasoning.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::High, Level::Max]
        );
        assert_eq!(reasoning.default, None, "an unknown default is no default");
    }

    #[test]
    fn list_an_odd_shape_is_no_data_not_a_failed_list() {
        let body = br#"{"data":[
            {"id":"a","reasoning":true},
            {"id":"b","capabilities":["tool_use"]},
            {"id":"c","effort":"high"},
            {"id":"d","effort":{"supported_levels":["low","high"],"default_level":"high"}}
        ]}"#;
        let models = read_list(body).expect("the list still parses");
        assert_eq!(models.len(), 4);
        assert!(models[..3].iter().all(|entry| entry.reasoning.is_none()));
        assert!(models[3].reasoning.is_some());
    }
}
