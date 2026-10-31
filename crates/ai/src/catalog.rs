//! The model list of a provider and its cache file.

pub mod list;
pub mod models_dev;

use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::http::{ExtraHeaders, client};
use crate::levels::Reasoning;
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
    /// The reasoning levels of the model, when a source lists them. A cache
    /// file written before this field has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// Whether the model takes images: `Some(true)` when the catalogue's input
    /// list names `image`, `Some(false)` when it lists inputs without it, and
    /// `None` when nothing lists an input list. A cache file written before
    /// this field has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
}

impl Entry {
    /// Fills the reasoning plan of this entry from `source` when the provider's
    /// own list said nothing about it: the precedence the price and the limits
    /// follow too.
    pub fn fill_reasoning(&mut self, source: &Entry) {
        if self.reasoning.is_none() {
            self.reasoning.clone_from(&source.reasoning);
        }
    }
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

/// Reads one provider's `/models` reply into entries.
pub type ReadList = fn(&[u8]) -> Result<Vec<Entry>, serde_json::Error>;

/// `raw` read as `T`, or `None` when it has another shape.
pub(crate) fn shaped<'a, T: Deserialize<'a>>(raw: Option<&'a RawValue>) -> Option<T> {
    serde_json::from_str(raw?.get()).ok()
}

/// `price` when every part is a finite amount of at least 0. A router lists
/// `-1` for a price that varies, and a bad price is no price at all.
pub(crate) fn sane(price: Price) -> Option<Price> {
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

/// Reads one price, written as text or as a number. Anything else, and text
/// that is not a number, is `None`.
pub(crate) fn amount<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
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

/// Reads the entries of an OpenAI-style `{"data": [...]}` reply, converting each
/// item to an [`Entry`] as it is read. Other top-level keys are ignored.
///
/// The item type is monomorphized per provider, so each provider's own shape
/// converts as serde reads it, with no intermediate list of items.
pub(crate) fn data<'a, T>(body: &'a [u8]) -> Result<Vec<Entry>, serde_json::Error>
where
    T: Deserialize<'a> + Into<Entry> + 'a,
{
    struct Data<'a, T>(PhantomData<&'a T>);

    /// A top-level key, read as an identifier so it costs no allocation.
    #[derive(Deserialize)]
    #[serde(field_identifier, rename_all = "lowercase")]
    enum Key {
        Data,
        #[serde(other)]
        Other,
    }

    impl<'a, T: Deserialize<'a> + Into<Entry> + 'a> Visitor<'a> for Data<'a, T> {
        type Value = Vec<Entry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a model list with a data array")
        }

        fn visit_map<A: MapAccess<'a>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = None;
            while let Some(key) = map.next_key::<Key>()? {
                match key {
                    Key::Data => entries = Some(map.next_value_seed(Items::<T>(PhantomData))?),
                    Key::Other => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok(entries.unwrap_or_default())
        }
    }

    struct Items<'a, T>(PhantomData<&'a T>);

    impl<'a, T: Deserialize<'a> + Into<Entry> + 'a> Visitor<'a> for Items<'a, T> {
        type Value = Vec<Entry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a list of models")
        }

        fn visit_seq<A: SeqAccess<'a>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(item) = seq.next_element::<T>()? {
                entries.push(item.into());
            }
            Ok(entries)
        }
    }

    impl<'a, T: Deserialize<'a> + Into<Entry> + 'a> serde::de::DeserializeSeed<'a> for Items<'a, T> {
        type Value = Vec<Entry>;

        fn deserialize<D: Deserializer<'a>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_seq(self)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let entries = deserializer.deserialize_map(Data::<T>(PhantomData))?;
    deserializer.end()?;
    Ok(entries)
}

/// Fetches `{base_url}{models_path}`. No error contains the key.
///
/// `credential` is `None` for a provider that lists its models to anyone.
/// `read` reads the reply in the provider's own shape.
pub async fn fetch(
    base_url: &str,
    models_path: &str,
    auth: Auth,
    credential: Option<&crate::key::Credential>,
    headers: Vec<(String, String)>,
    read: ReadList,
) -> Result<Vec<Entry>, String> {
    let url = format!("{}{models_path}", base_url.trim_end_matches('/'));
    let mut request = ExtraHeaders::new(headers).apply(client().get(url));
    // The credential names its own header: a key is a bearer or `x-api-key`,
    // and a saved login refreshes its token and is always a bearer.
    if let Some(credential) = credential {
        let (name, value) = credential.header().await?;
        request = request.header(name, value);
    }
    if auth == Auth::XApiKey {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let response = request.send().await.map_err(describe)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let body = response.bytes().await.map_err(describe)?;
    read(&body).map_err(|error| format!("bad model list: {error}"))
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
    use crate::levels::Level;

    #[derive(Deserialize)]
    struct Id {
        id: String,
    }

    impl From<Id> for Entry {
        fn from(item: Id) -> Entry {
            Entry {
                id: item.id,
                context_window: None,
                max_output: None,
                price: None,
                price_source: None,
                reasoning: None,
                images: None,
            }
        }
    }

    #[test]
    fn data_skips_other_keys_reads_no_data_as_empty_and_fails_a_bad_item() {
        let ids = |body: &[u8]| -> Vec<String> {
            data::<Id>(body)
                .expect("parses")
                .into_iter()
                .map(|entry| entry.id)
                .collect()
        };
        assert_eq!(
            ids(br#"{"object":"list","links":{"x":[1]},"data":[{"id":"a"},{"id":"b"}],"total":2}"#),
            ["a", "b"]
        );
        assert!(ids(br#"{"object":"list"}"#).is_empty());
        assert!(data::<Id>(br#"{"data":[{"id":1}]}"#).is_err());
        assert!(data::<Id>(br#"{"data":[]} trailing"#).is_err());
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
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes one file into a temp folder"
    )]
    fn usage_cost_old_cache_without_price_loads() {
        let temp = tempfile::tempdir().expect("makes a temp folder");
        let dir = temp.path();
        std::fs::write(
            path(dir, "fi"),
            br#"{"fetched_at":1,"models":[{"id":"m","context_window":8000,"max_output":null}]}"#,
        )
        .expect("writes an old cache");
        let cache = read(dir, "fi").expect("an old cache loads");
        assert_eq!(cache.models[0].price, None);
        assert_eq!(cache.models[0].context_window, Some(8000));
        assert_eq!(
            cache.models[0].reasoning, None,
            "an old cache names no reasoning levels"
        );
        assert_eq!(
            cache.models[0].images, None,
            "an old cache says nothing about images"
        );
    }

    #[test]
    fn catalog_cache_round_trips() {
        let temp = tempfile::tempdir().expect("makes a temp folder");
        let dir = temp.path();
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
                reasoning: None,
                images: Some(true),
            }],
        };
        assert_eq!(read(dir, "fi"), None);
        write(dir, "fi", &cache).expect("writes");
        assert_eq!(read(dir, "fi"), Some(cache));
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

    /// An entry with no limits and no price, for the reasoning tests.
    fn bare(id: &str, reasoning: Option<Reasoning>) -> Entry {
        Entry {
            id: id.into(),
            context_window: None,
            max_output: None,
            price: None,
            price_source: None,
            reasoning,
            images: None,
        }
    }

    #[test]
    fn reasoning_levels_the_provider_list_wins_over_the_catalogue() {
        let listed = bare("m", Some(Reasoning::from_words(["low", "high"])));
        let catalogue = bare(
            "m",
            Some(Reasoning {
                off: Some(true),
                ..Reasoning::from_words(["medium"])
            }),
        );
        let mut entry = listed.clone();
        entry.fill_reasoning(&catalogue);
        assert_eq!(
            entry.reasoning, listed.reasoning,
            "the provider's own list wins"
        );
        let mut missing = bare("m", None);
        missing.fill_reasoning(&catalogue);
        assert_eq!(
            missing.reasoning, catalogue.reasoning,
            "a list that says nothing about reasoning is filled"
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads one file back from a temp folder"
    )]
    fn reasoning_levels_the_cache_writes_the_level_names() {
        let temp = tempfile::tempdir().expect("makes a temp folder");
        let dir = temp.path();
        let cache = Cache {
            fetched_at: 1,
            models: vec![bare(
                "m",
                Some(Reasoning {
                    off: Some(false),
                    mandatory: true,
                    default: Some(Level::High),
                    ..Reasoning::from_words(["low", "high"])
                }),
            )],
        };
        write(dir, "fi", &cache).expect("writes");
        let text = std::fs::read_to_string(path(dir, "fi")).expect("reads the cache text");
        assert!(
            text.contains(
                r#""reasoning":{"levels":["low","high"],"off":false,"mandatory":true,"default":"high"}"#
            ),
            "{text}"
        );
        assert_eq!(read(dir, "fi"), Some(cache));
    }
}
