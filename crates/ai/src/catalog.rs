//! The model list of a provider and its cache file.

pub mod models_dev;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::http::{ExtraHeaders, client};
use crate::types::Usage;

/// How long a cached list counts as fresh, in seconds.
pub const FRESH_SECS: u64 = 24 * 60 * 60;

/// What one model costs in USD per token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// USD per input token.
    pub input: f64,
    /// USD per output token.
    pub output: f64,
    /// USD per input token read from the prompt cache.
    pub cache_read: f64,
    /// USD per input token written to the prompt cache.
    pub cache_write: f64,
}

// `f64` has no `Eq`, so the marker is written out by hand. A listing never
// carries a `NaN` price, and two prices meet only in a cache round trip.
impl Eq for Price {}

impl Price {
    /// What `usage` costs at this price, in USD.
    pub fn cost(&self, usage: &Usage) -> f64 {
        self.input * usage.input_tokens as f64
            + self.output * usage.output_tokens as f64
            + self.cache_read * usage.cache_read_tokens as f64
            + self.cache_write * usage.cache_write_tokens as f64
    }
}

/// Where the price of an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// The provider's own model list.
    Provider,
    /// The models.dev catalogue, whose peak hours multiply a price.
    ModelsDev,
}

/// One model a provider lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The model id.
    pub id: String,
    /// The context window in tokens, when the provider says.
    pub context_window: Option<u64>,
    /// The cap on reply tokens, when the provider says.
    pub max_output: Option<u64>,
    /// The price per token, when the provider lists one. A cache file written
    /// before prices were read has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// Where `price` came from, when it is known. A price from the models.dev
    /// catalogue doubles in that catalogue's peak hours; a price a provider
    /// lists never does. A cache file written before this field has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_source: Option<PriceSource>,
}

/// A cached model list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    /// When the list was fetched, in Unix seconds.
    pub fetched_at: u64,
    /// The listed models.
    pub models: Vec<Entry>,
}

impl Cache {
    /// Whether the list is younger than [`FRESH_SECS`] at `now`.
    pub fn is_fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.fetched_at) < FRESH_SECS
    }
}

/// How the key goes in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>` with the Anthropic version header.
    XApiKey,
}

#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    data: Vec<Item>,
    /// The ChatGPT plan shape, which lists each entry with a visibility.
    models: Option<Vec<PlanItem>>,
}

/// One entry of the ChatGPT plan shape.
#[derive(Deserialize)]
struct PlanItem {
    slug: String,
    visibility: Option<String>,
}

#[derive(Deserialize)]
struct Item {
    id: String,
    context_length: Option<u64>,
    /// DeepSeek's own name for `context_length`.
    context_window: Option<u64>,
    top_provider: Option<TopProvider>,
    /// DeepSeek's own name for `top_provider.max_completion_tokens`.
    max_output_tokens: Option<u64>,
    pricing: Option<Pricing>,
}

#[derive(Deserialize)]
struct TopProvider {
    max_completion_tokens: Option<u64>,
}

/// The `pricing` object of one listed model. OpenRouter writes each price as a
/// string, so a listing is read as either text or a number.
#[derive(Deserialize)]
struct Pricing {
    #[serde(default, deserialize_with = "amount")]
    prompt: Option<f64>,
    #[serde(default, deserialize_with = "amount")]
    completion: Option<f64>,
    #[serde(default, deserialize_with = "amount")]
    input_cache_read: Option<f64>,
    #[serde(default, deserialize_with = "amount")]
    input_cache_write: Option<f64>,
}

impl Pricing {
    /// The price, or `None` when the listing leaves out a token price or writes
    /// one that is not a number. A cache price that is missing or bad counts as
    /// free, which is the common case for a provider that does not say.
    /// `None` unless every price is a finite amount of at least 0; routers
    /// list `-1` for a price that varies.
    fn price(self) -> Option<Price> {
        let price = Price {
            input: self.prompt?,
            output: self.completion?,
            cache_read: self.input_cache_read.unwrap_or(0.0),
            cache_write: self.input_cache_write.unwrap_or(0.0),
        };
        [
            price.input,
            price.output,
            price.cache_read,
            price.cache_write,
        ]
        .iter()
        .all(|amount| amount.is_finite() && *amount >= 0.0)
        .then_some(price)
    }
}

/// Reads one price, written as text or as a number. Anything else, and text
/// that is not a number, is `None`.
fn amount<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Amount;

    impl serde::de::Visitor<'_> for Amount {
        type Value = Option<f64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a price as a number or a string")
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
            Ok(value.trim().parse().ok())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(Amount)
}

/// Reads the entries of an OpenAI-style `{"data": [...]}` reply, or of the
/// ChatGPT plan shape, which lists `models` with a visibility and a slug.
pub fn parse(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    let listing: Listing = serde_json::from_slice(body)?;
    if let Some(models) = listing.models {
        return Ok(models
            .into_iter()
            .filter(|model| model.visibility.as_deref() == Some("list"))
            .map(|model| Entry {
                id: model.slug,
                context_window: None,
                max_output: None,
                price: None,
                price_source: None,
            })
            .collect());
    }
    Ok(listing
        .data
        .into_iter()
        .map(|item| {
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
            }
        })
        .collect())
}

/// Fetches `{base_url}/models`. No error contains the key.
pub async fn fetch(
    base_url: &str,
    models_path: &str,
    auth: Auth,
    credential: &crate::key::Credential,
    headers: Vec<(String, String)>,
) -> Result<Vec<Entry>, String> {
    let url = format!("{}{models_path}", base_url.trim_end_matches('/'));
    let mut request = ExtraHeaders::new(headers).apply(client().get(url));
    // The credential names its own header: a key is a bearer or `x-api-key`,
    // and a saved login refreshes its token and is always a bearer.
    let (name, value) = credential.header().await?;
    request = request.header(name, value);
    if auth == Auth::XApiKey {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let response = request.send().await.map_err(describe)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let body = response.bytes().await.map_err(describe)?;
    parse(&body).map_err(|error| format!("bad model list: {error}"))
}

fn describe(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

/// The cache file of provider `name` in `dir`.
pub fn path(dir: &Path, name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.json"))
}

/// Reads the cache of `name`, or `None` when it is missing or unreadable.
#[expect(
    clippy::disallowed_methods,
    reason = "the cache is one small file, read without the network"
)]
pub fn read(dir: &Path, name: &str) -> Option<Cache> {
    let bytes = std::fs::read(path(dir, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes the cache of `name` through a temp file and a rename.
#[expect(
    clippy::disallowed_methods,
    reason = "the cache is one small file, written once per fetch"
)]
pub fn write(dir: &Path, name: &str, cache: &Cache) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = path(dir, name);
    let temp = target.with_extension(format!("json.tmp{}", std::process::id()));
    let bytes = serde_json::to_vec(cache).map_err(std::io::Error::other)?;
    std::fs::write(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, &target))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_preset_model_list_keeps_listed_slugs() {
        let body = br#"{"models":[{"slug":"gpt-5","visibility":"list"},{"slug":"gpt-5-internal","visibility":"hidden"}]}"#;
        let entries = parse(body).expect("the plan shape parses");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].id, "gpt-5");
        assert_eq!(entries[0].context_window, None);
        assert_eq!(entries[0].price, None);
        // The older `data` shape still reads as it did.
        let old = parse(br#"{"data":[{"id":"a","context_length":8000}]}"#)
            .expect("the data shape parses");
        assert_eq!(old[0].id, "a");
        assert_eq!(old[0].context_window, Some(8000));
    }

    #[test]
    fn catalog_parse_reads_the_limits_when_present() {
        let body = br#"{"data":[
            {"id":"a","context_length":128000,"top_provider":{"max_completion_tokens":4096},"x":1},
            {"id":"b"},
            {"id":"c","top_provider":{}}
        ]}"#;
        let entries = parse(body).expect("parses");
        let entry = |id: &str, window, output| Entry {
            id: id.into(),
            context_window: window,
            max_output: output,
            price: None,
            price_source: None,
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
    fn catalog_limit_names_read_the_deepseek_fields() {
        let body = br#"{"data":[{"id":"deepseek-flash","context_window":1048576,"max_output_tokens":393216}]}"#;
        let entries = parse(body).expect("parses");
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
        let entries = parse(both).expect("parses");
        assert_eq!(entries[0].context_window, Some(10));
        assert_eq!(entries[0].max_output, Some(3));
        assert_eq!(entries[0].price_source, Some(PriceSource::Provider));
    }

    #[test]
    fn usage_cost_is_parsed_from_the_pricing_object() {
        let body = br#"{"data":[
            {"id":"priced","pricing":{
                "prompt":"0.000001","completion":"0.000002",
                "input_cache_read":"0.0000001","input_cache_write":"0.0000012"
            }},
            {"id":"no-cache-prices","pricing":{"prompt":"0.000003","completion":"bad"}},
            {"id":"prompt-only","pricing":{"prompt":"0.000001"}},
            {"id":"empty","pricing":{}}
        ]}"#;
        let entries = parse(body).expect("parses");
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
        let entries = parse(cache_prices).expect("parses");
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
    fn usage_cost_from_a_price_counts_every_token_kind() {
        let price = Price {
            input: 0.000_001,
            output: 0.000_002,
            cache_read: 0.000_000_1,
            cache_write: 0.000_001_2,
        };
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 2000,
            cache_write_tokens: 1000,
            ..Usage::default()
        };
        let cost = price.cost(&usage);
        let expected =
            1000.0 * 0.000_001 + 500.0 * 0.000_002 + 2000.0 * 0.000_000_1 + 1000.0 * 0.000_001_2;
        assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");
        assert!((cost - 0.0034).abs() < 1e-12, "{cost}");
    }

    #[test]
    fn usage_cost_ignores_a_negative_or_non_finite_price() {
        let body = br#"{"data":[
            {"id":"router","pricing":{"prompt":"-1","completion":"-1"}},
            {"id":"nan","pricing":{"prompt":"nan","completion":"0.000002"}},
            {"id":"inf","pricing":{"prompt":"0.000001","completion":"inf"}},
            {"id":"bad-cache","pricing":{"prompt":"0","completion":"0","input_cache_read":"-1"}}
        ]}"#;
        let entries = parse(body).expect("parses");
        assert!(
            entries.iter().all(|entry| entry.price.is_none()),
            "{entries:?}"
        );
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "the test removes its folder")]
    fn usage_cost_old_cache_without_price_loads() {
        let dir = std::env::temp_dir().join(format!("reagent-catalog-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the folder");
        std::fs::write(
            path(&dir, "fi"),
            br#"{"fetched_at":1,"models":[{"id":"m","context_window":8000,"max_output":null}]}"#,
        )
        .expect("writes an old cache");
        let cache = read(&dir, "fi").expect("an old cache loads");
        assert_eq!(cache.models[0].price, None);
        assert_eq!(cache.models[0].context_window, Some(8000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_parse_rejects_text_that_is_not_json() {
        assert!(parse(b"nope").is_err());
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "the test removes its folder")]
    fn catalog_cache_round_trips() {
        let dir = std::env::temp_dir().join(format!("reagent-catalog-{}", std::process::id()));
        let cache = Cache {
            fetched_at: 1_700_000_000,
            models: vec![Entry {
                id: "m".into(),
                context_window: Some(8000),
                max_output: None,
                price: Some(Price {
                    input: 0.000_001,
                    output: 0.000_002,
                    cache_read: 0.0,
                    cache_write: 0.0,
                }),
                price_source: Some(PriceSource::Provider),
            }],
        };
        assert_eq!(read(&dir, "fi"), None);
        write(&dir, "fi", &cache).expect("writes");
        assert_eq!(read(&dir, "fi"), Some(cache));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_cache_is_fresh_for_a_day() {
        let cache = Cache {
            fetched_at: 1000,
            models: Vec::new(),
        };
        assert!(cache.is_fresh(1000 + FRESH_SECS - 1));
        assert!(!cache.is_fresh(1000 + FRESH_SECS));
        assert!(cache.is_fresh(500));
    }

    #[test]
    fn catalog_path_keeps_the_name_inside_the_folder() {
        assert_eq!(
            path(Path::new("/c"), "../x/y"),
            PathBuf::from("/c/.._x_y.json")
        );
    }
}
