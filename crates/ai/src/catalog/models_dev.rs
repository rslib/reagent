//! The models.dev catalogue: the prices and the limits of a model.
//!
//! One reply of about 6 MB holds every provider. Only the providers asked for
//! are built; the rest is skipped token by token, so a full reply costs a
//! single pass and no tree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::{Entry, FRESH_SECS, Price, PriceSource, describe};
use crate::levels::Reasoning;

/// The catalogue URL.
pub const URL: &str = "https://models.dev/api.json";

/// The cache file name in the models cache folder.
const NAME: &str = "models.dev";

/// The tokens of one million tokens: the catalogue prices in USD per million.
const PER_MILLION: f64 = 1_000_000.0;

/// The `cost` object of one catalogue model, in USD per 1M tokens.
#[derive(Debug, Default, Deserialize)]
struct Cost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

impl Cost {
    /// The price per token, or `None` when a part is not a finite amount of at
    /// least 0. A part the catalogue leaves out is free.
    fn price(self) -> Option<Price> {
        let price = Price {
            input: self.input.unwrap_or(0.0) / PER_MILLION,
            output: self.output.unwrap_or(0.0) / PER_MILLION,
            cache_read: self.cache_read.unwrap_or(0.0) / PER_MILLION,
            cache_write: self.cache_write.unwrap_or(0.0) / PER_MILLION,
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

/// The `limit` object of one catalogue model.
#[derive(Debug, Default, Deserialize)]
struct Limit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

/// The `modalities` object of one catalogue model: the inputs it takes.
#[derive(Debug, Default, Deserialize)]
struct Modalities {
    /// The input kinds the model takes, which holds `image` for a model that
    /// takes images.
    #[serde(default)]
    input: Option<Vec<String>>,
}

impl Modalities {
    /// Whether the model takes images: `Some(true)` when the input list names
    /// `image`, `Some(false)` when it lists inputs without it, and `None` when
    /// it lists no input list.
    fn images(&self) -> Option<bool> {
        let input = self.input.as_ref()?;
        Some(input.iter().any(|kind| kind == "image"))
    }
}

/// One catalogue model, with only the fields reagent reads. Every other field
/// is ignored.
#[derive(Debug, Default, Deserialize)]
struct Model {
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    limit: Option<Limit>,
    /// The input kinds the catalogue lists for it.
    #[serde(default)]
    modalities: Option<Modalities>,
    /// The reasoning levels and the toggle the catalogue lists for it.
    #[serde(default)]
    reasoning_options: Vec<ReasoningOption>,
}

impl Model {
    /// The entry for `id`. A model with no `cost` object has no price.
    fn entry(self, id: String) -> Entry {
        let price = self.cost.and_then(Cost::price);
        let (context_window, max_output) = match self.limit {
            Some(limit) => (limit.context, limit.output),
            None => (None, None),
        };
        Entry {
            id,
            context_window,
            max_output,
            price_source: price.map(|_| PriceSource::ModelsDev),
            price,
            reasoning: reasoning(self.reasoning_options),
            images: self.modalities.as_ref().and_then(Modalities::images),
        }
    }
}

/// One `reasoning_options` entry: an `effort` list of level names, a `toggle`,
/// or a `budget_tokens` range, which reagent does not read yet.
#[derive(Debug, Deserialize)]
struct ReasoningOption {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    values: Vec<Word>,
}

/// One `values` entry: a word, or anything else, which is skipped so a
/// catalogue that writes a number in the list still loads.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Word {
    /// The word itself.
    Text(String),
    /// Anything else.
    Other(IgnoredAny),
}

impl Word {
    /// The word itself, or `None` for an entry that is not a word.
    fn text(self) -> Option<String> {
        match self {
            Word::Text(text) => Some(text),
            Word::Other(_) => None,
        }
    }
}

/// What the `reasoning_options` of one catalogue model say: the levels an
/// `effort` entry names, with a `none` value turning thinking off, and a
/// `toggle` turning it off too. A `budget_tokens` range says nothing reagent
/// reads yet.
fn reasoning(options: Vec<ReasoningOption>) -> Option<Reasoning> {
    let mut words = Vec::new();
    let mut off = None;
    for option in options {
        match option.kind.as_deref() {
            Some("toggle") => off = Some(true),
            Some("effort") => words.extend(option.values.into_iter().filter_map(Word::text)),
            _ => {}
        }
    }
    let mut plan = Reasoning::from_words(words.iter().map(String::as_str));
    plan.off = plan.off.or(off);
    (!plan.is_empty()).then_some(plan)
}

/// The `models` map of one provider object: the model id is the key.
struct Models(Vec<Entry>);

impl<'de> Deserialize<'de> for Models {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ModelsVisitor;

        impl<'de> Visitor<'de> for ModelsVisitor {
            type Value = Models;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map of model id to model")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Models, A::Error> {
                let mut models: BTreeMap<String, Model> = BTreeMap::new();
                while let Some(id) = map.next_key::<String>()? {
                    models.insert(id, map.next_value()?);
                }
                Ok(Models(
                    models
                        .into_iter()
                        .map(|(id, model)| model.entry(id))
                        .collect(),
                ))
            }
        }

        deserializer.deserialize_map(ModelsVisitor)
    }
}

/// One provider object: its `models` map, and nothing else.
struct Provider(Vec<Entry>);

impl<'de> Deserialize<'de> for Provider {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProviderVisitor;

        impl<'de> Visitor<'de> for ProviderVisitor {
            type Value = Provider;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a provider object with a models map")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Provider, A::Error> {
                let mut models = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "models" {
                        models = map.next_value::<Models>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(Provider(models))
            }
        }

        deserializer.deserialize_map(ProviderVisitor)
    }
}

/// The providers in `ids`: a provider that is not asked for is skipped
/// without building its models.
struct Wanted<'a>(&'a [&'a str]);

impl<'de> Visitor<'de> for Wanted<'_> {
    type Value = Vec<(String, Vec<Entry>)>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a catalogue keyed by provider id")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut found = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if !self.0.contains(&key.as_str()) {
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            let Provider(entries) = map.next_value::<Provider>()?;
            found.push((key, entries));
        }
        Ok(found)
    }
}

/// The entries of each provider in `ids`, with the price per token and the
/// limits the catalogue lists. A provider the catalogue does not have is left
/// out; so is every field reagent does not read.
pub fn extract(body: &[u8], ids: &[&str]) -> Result<Vec<(String, Vec<Entry>)>, serde_json::Error> {
    let mut reader = serde_json::Deserializer::from_slice(body);
    let found = serde::Deserializer::deserialize_map(&mut reader, Wanted(ids))?;
    reader.end()?;
    Ok(found)
}

/// The cached catalogue: only the providers that were asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Cached {
    /// When it was fetched, in Unix seconds.
    fetched_at: u64,
    /// The entries of each provider, by provider id.
    providers: BTreeMap<String, Vec<Entry>>,
}

impl Cached {
    /// Whether the catalogue is younger than [`FRESH_SECS`] at `now`.
    fn is_fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.fetched_at) < FRESH_SECS
    }

    /// The lists of the `ids` it holds.
    fn pick(&self, ids: &[&str]) -> BTreeMap<String, Vec<Entry>> {
        ids.iter()
            .filter_map(|id| {
                self.providers
                    .get(*id)
                    .map(|entries| ((*id).to_string(), entries.clone()))
            })
            .collect()
    }
}

/// The cache file of the catalogue in `dir`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(format!("{NAME}.json"))
}

/// Reads the cached catalogue, or `None` when it is missing or unreadable.
#[expect(
    clippy::disallowed_methods,
    reason = "the cache is one small file, read without the network"
)]
fn read(dir: &Path) -> Option<Cached> {
    let bytes = std::fs::read(path(dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes the cached catalogue through a temp file and a rename.
#[expect(
    clippy::disallowed_methods,
    reason = "the cache is one small file, written once per fetch"
)]
fn write(dir: &Path, cache: &Cached) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = path(dir);
    let temp = target.with_extension(format!("json.tmp{}", std::process::id()));
    let bytes = serde_json::to_vec(cache).map_err(std::io::Error::other)?;
    std::fs::write(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, &target))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
}

/// What the catalogue says about the providers in `ids`: the cache when it is
/// fresh and holds every id, else one fetch of `url`. A failed fetch falls back
/// to a stale cache, and without one it is the error.
///
/// The caller owns the deadline: the crate has no timer without a runtime.
pub async fn load(
    dir: &Path,
    ids: &[&str],
    now: u64,
    url: &str,
) -> Result<BTreeMap<String, Vec<Entry>>, String> {
    let cached = read(dir);
    if let Some(cached) = &cached
        && cached.is_fresh(now)
        && ids.iter().all(|id| cached.providers.contains_key(*id))
    {
        return Ok(cached.pick(ids));
    }
    let fresh = cached.as_ref().filter(|cached| cached.is_fresh(now));
    match fetch(dir, ids, now, url, fresh).await {
        Ok(found) => Ok(found),
        Err(error) => match &cached {
            Some(cached) => Ok(cached.pick(ids)),
            None => Err(error),
        },
    }
}

/// Fetches `url`, keeps the providers in `ids` and writes the cache. The cache
/// holds an empty list for a provider the catalogue does not have, so the next
/// run does not fetch the whole catalogue again for it. The providers of a
/// `fresh` cache stay in it, so listing one provider does not drop another.
async fn fetch(
    dir: &Path,
    ids: &[&str],
    now: u64,
    url: &str,
    fresh: Option<&Cached>,
) -> Result<BTreeMap<String, Vec<Entry>>, String> {
    let response = crate::http::client()
        .get(url)
        .send()
        .await
        .map_err(describe)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let body = response.bytes().await.map_err(describe)?;
    let mut found: BTreeMap<String, Vec<Entry>> = ids
        .iter()
        .map(|id| ((*id).to_string(), Vec::new()))
        .collect();
    for (id, entries) in extract(&body, ids).map_err(|error| format!("bad catalogue: {error}"))? {
        found.insert(id, entries);
    }
    let mut providers = fresh
        .map(|cached| cached.providers.clone())
        .unwrap_or_default();
    providers.extend(
        found
            .iter()
            .map(|(id, entries)| (id.clone(), entries.clone())),
    );
    // A cache that cannot be written costs the next run a fetch, never this
    // one's prices.
    let _ = write(
        dir,
        &Cached {
            fetched_at: now,
            providers,
        },
    );
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::levels::Level;

    /// A trimmed real catalogue reply: two providers, with the fields reagent
    /// does not read kept so a bad field would be noticed.
    const SAMPLE: &str = r#"{
     "deepseek": {
      "id": "deepseek",
      "name": "DeepSeek",
      "models": {
       "deepseek-flash": {
        "id": "deepseek-flash",
        "name": "DeepSeek V4.1 Flash 1017",
        "reasoning": true,
        "reasoning_options": [{"type": "toggle"}],
        "limit": {"context": 1048576, "output": 393216},
        "modalities": {"input": ["text", "image"], "output": ["text"]},
        "cost": {"input": 0.15, "output": 0.6, "reasoning": 0.6, "cache_read": 0.003},
        "canonical_model_id": "deepseek/deepseek-v4.1-flash-1017"
       },
       "deepseek-v4-pro": {
        "id": "deepseek-v4-pro",
        "limit": {"context": 1048576, "output": 393216},
        "cost": {"input": 0.66, "output": 1.98, "cache_read": 0.022}
       }
      }
     },
     "openai": {
      "id": "openai",
      "models": {
       "gpt-6-sol": {
        "id": "gpt-6-sol",
        "limit": {"context": 1050000, "input": 922000, "output": 128000},
        "cost": {"input": 2, "output": 10, "cache_read": 0.2, "cache_write": 2.5,
                 "tiers": [{"input": 4, "output": 15}]}
       }
      }
     }
}"#;

    #[test]
    fn models_dev_extract_reads_the_price_and_the_limits() {
        let body = SAMPLE.as_bytes();
        let found = extract(body, &["deepseek"]).expect("the sample parses");
        assert_eq!(found.len(), 1);
        let (id, entries) = &found[0];
        assert_eq!(id, "deepseek");
        let flash = entries
            .iter()
            .find(|entry| entry.id == "deepseek-flash")
            .expect("the flash model");
        assert_eq!(flash.context_window, Some(1_048_576));
        assert_eq!(flash.max_output, Some(393_216));
        let price = flash.price.expect("a price");
        assert!((price.input - 0.15e-6).abs() < 1e-15, "{price:?}");
        assert!((price.output - 0.6e-6).abs() < 1e-15, "{price:?}");
        assert!((price.cache_read - 0.003e-6).abs() < 1e-15, "{price:?}");
        assert!((price.cache_write - 0.0).abs() < 1e-15, "{price:?}");
        assert_eq!(flash.price_source, Some(PriceSource::ModelsDev));
        // A price the catalogue lists without a tiers rule is the price.
        let (_, openai) = &extract(body, &["openai"]).expect("parses")[0];
        let sol = &openai[0];
        assert_eq!(sol.context_window, Some(1_050_000));
        assert_eq!(sol.max_output, Some(128_000));
        assert!((sol.price.expect("a price").cache_write - 2.5e-6).abs() < 1e-15);
    }

    #[test]
    fn models_dev_extract_leaves_out_an_unknown_provider_and_a_missing_cost() {
        assert!(
            extract(SAMPLE.as_bytes(), &["nope"])
                .expect("parses")
                .is_empty(),
            "an unknown provider gives nothing"
        );
        let body = br#"{"fi":{"models":{"m":{"id":"m","limit":{"context":8}}}}}"#;
        let found = extract(body, &["fi"]).expect("parses");
        assert_eq!(found[0].1[0].context_window, Some(8));
        assert_eq!(found[0].1[0].price, None, "no cost object is no price");
        assert_eq!(found[0].1[0].price_source, None);
    }

    #[test]
    fn reasoning_levels_models_dev_reads_effort_and_toggle() {
        // The sample's deepseek-flash lists a bare toggle: thinking can be
        // turned off, and no level is named.
        let found = extract(SAMPLE.as_bytes(), &["deepseek"]).expect("parses");
        let flash = found[0]
            .1
            .iter()
            .find(|entry| entry.id == "deepseek-flash")
            .expect("the flash model");
        let reasoning = flash.reasoning.as_ref().expect("a toggle plan");
        assert_eq!(reasoning.levels.iter().count(), 0);
        assert_eq!(reasoning.off, Some(true));
        assert!(!reasoning.mandatory);
        // A model with no reasoning options has no plan.
        let pro = found[0]
            .1
            .iter()
            .find(|entry| entry.id == "deepseek-v4-pro")
            .expect("the pro model");
        assert_eq!(pro.reasoning, None);

        // An effort entry names the levels; a none value turns thinking off,
        // a number is skipped and a budget range says nothing yet.
        let body = br#"{"fi":{"models":{"m":{"id":"m","reasoning_options":[
            {"type":"effort","values":["low","medium","none",7]},
            {"type":"budget_tokens","min":1024,"max":32000}
        ]}}}}"#;
        let found = extract(body, &["fi"]).expect("parses");
        let reasoning = found[0].1[0].reasoning.as_ref().expect("an effort plan");
        assert_eq!(
            reasoning.levels.iter().collect::<Vec<_>>(),
            [Level::Low, Level::Medium]
        );
        assert_eq!(reasoning.off, Some(true), "a none value turns thinking off");
        assert_eq!(reasoning.default, None);
    }

    #[test]
    fn image_input_models_dev_reads_the_input_list() {
        // An input list that names `image` lets the model take images.
        let found = extract(SAMPLE.as_bytes(), &["deepseek"]).expect("parses");
        let flash = found[0]
            .1
            .iter()
            .find(|entry| entry.id == "deepseek-flash")
            .expect("the flash model");
        assert_eq!(flash.images, Some(true));
        // An input list without it says the model takes none.
        let body = br#"{"fi":{"models":{"m":{"id":"m","modalities":{"input":["text"]}}}}}"#;
        let found = extract(body, &["fi"]).expect("parses");
        assert_eq!(found[0].1[0].images, Some(false));
        // A model with no input list says nothing about images.
        let body = br#"{"fi":{"models":{"m":{"id":"m"}}}}"#;
        let found = extract(body, &["fi"]).expect("parses");
        assert_eq!(found[0].1[0].images, None);
    }

    #[test]
    fn models_dev_extract_refuses_text_that_is_not_json() {
        assert!(extract(b"nope", &["fi"]).is_err());
    }

    /// A temp folder for the catalogue cache.
    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("reagent-models-dev-{tag}-{}", std::process::id()))
    }

    /// Answers every request with `body`, and counts them.
    async fn serve(body: &'static str) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port");
        let address = listener.local_addr().expect("the bound address");
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&count);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}/api.json"), count)
    }

    #[tokio::test]
    #[expect(clippy::disallowed_methods, reason = "the test removes its folder")]
    async fn models_dev_a_fresh_cache_makes_no_request() {
        let dir = temp_dir("fresh");
        let _ = std::fs::remove_dir_all(&dir);
        let (url, count) = serve(SAMPLE_BODY).await;
        let found = load(&dir, &["deepseek"], 1_000, &url)
            .await
            .expect("the first load fetches");
        assert_eq!(
            found["deepseek"][0].id, "deepseek-flash",
            "the first load fills"
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The cache written a moment ago still counts as fresh at that time.
        let again = load(&dir, &["deepseek"], 1_000 + FRESH_SECS - 1, &url)
            .await
            .expect("the cache loads");
        assert_eq!(again, found);
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the fresh cache is used, not the network"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[expect(clippy::disallowed_methods, reason = "the test removes its folder")]
    async fn models_dev_a_stale_cache_is_refetched() {
        let dir = temp_dir("stale");
        let _ = std::fs::remove_dir_all(&dir);
        let (url, count) = serve(SAMPLE_BODY).await;
        load(&dir, &["deepseek"], 1_000, &url)
            .await
            .expect("the first load fetches");
        load(&dir, &["deepseek"], 1_000 + FRESH_SECS, &url)
            .await
            .expect("a stale cache is refetched");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A fresh cache that lacks an id is refetched too.
        load(&dir, &["deepseek", "openai"], 2_000, &url)
            .await
            .expect("a missing id refetches");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[expect(clippy::disallowed_methods, reason = "the test removes its folder")]
    async fn models_dev_a_fetch_error_without_a_cache_is_an_error() {
        let dir = temp_dir("error");
        let _ = std::fs::remove_dir_all(&dir);
        // A port that nothing listens on: the fetch fails to connect.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port");
        let address = listener.local_addr().expect("the bound address");
        drop(listener);
        let url = format!("http://{address}/api.json");
        let error = load(&dir, &["deepseek"], 1_000, &url)
            .await
            .expect_err("a fetch error without a cache is the error");
        assert!(!error.is_empty());
        // With a stale cache the error is not returned.
        let (url, _count) = serve(SAMPLE_BODY).await;
        load(&dir, &["deepseek"], 1_000, &url)
            .await
            .expect("the first load fetches");
        let stale = load(
            &dir,
            &["deepseek"],
            1_000 + FRESH_SECS,
            "http://127.0.0.1:1/api.json",
        )
        .await
        .expect("a stale cache answers a failed fetch");
        assert_eq!(stale["deepseek"][0].id, "deepseek-flash");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[expect(clippy::disallowed_methods, reason = "the test removes its folder")]
    async fn models_dev_the_cache_holds_only_the_ids_asked_for() {
        let dir = temp_dir("only");
        let _ = std::fs::remove_dir_all(&dir);
        let (url, _count) = serve(SAMPLE_BODY).await;
        load(&dir, &["openai", "nope"], 1_000, &url)
            .await
            .expect("the load fetches");
        let cache = read(&dir).expect("the cache is written");
        assert_eq!(cache.fetched_at, 1_000);
        assert!(cache.providers.contains_key("openai"));
        assert_eq!(cache.providers.get("nope"), Some(&Vec::new()));
        assert!(!cache.providers.contains_key("deepseek"));
        assert_eq!(path(&dir), dir.join("models.dev.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[expect(clippy::disallowed_methods, reason = "the test removes its folder")]
    async fn models_dev_a_second_provider_keeps_the_first_in_the_cache() {
        let dir = temp_dir("merge");
        let _ = std::fs::remove_dir_all(&dir);
        let (url, count) = serve(SAMPLE_BODY).await;
        load(&dir, &["deepseek"], 1_000, &url)
            .await
            .expect("the first load fetches");
        load(&dir, &["openai"], 1_500, &url)
            .await
            .expect("a new id fetches");
        let cache = read(&dir).expect("the cache is written");
        assert!(cache.providers.contains_key("deepseek"));
        assert!(cache.providers.contains_key("openai"));
        load(&dir, &["deepseek"], 1_600, &url)
            .await
            .expect("the merged cache answers");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sample as a reply body a fake server can serve.
    const SAMPLE_BODY: &str = SAMPLE;
}
