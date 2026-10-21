//! The model list of a provider and its cache file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::http::{ExtraHeaders, client};

/// How long a cached list counts as fresh, in seconds.
pub const FRESH_SECS: u64 = 24 * 60 * 60;

/// One model a provider lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The model id.
    pub id: String,
    /// The context window in tokens, when the provider says.
    pub context_window: Option<u64>,
    /// The cap on reply tokens, when the provider says.
    pub max_output: Option<u64>,
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
}

#[derive(Deserialize)]
struct Item {
    id: String,
    context_length: Option<u64>,
    top_provider: Option<TopProvider>,
}

#[derive(Deserialize)]
struct TopProvider {
    max_completion_tokens: Option<u64>,
}

/// Reads the entries of an OpenAI-style `{"data": [...]}` reply.
pub fn parse(body: &[u8]) -> Result<Vec<Entry>, serde_json::Error> {
    let listing: Listing = serde_json::from_slice(body)?;
    Ok(listing
        .data
        .into_iter()
        .map(|item| Entry {
            id: item.id,
            context_window: item.context_length,
            max_output: item.top_provider.and_then(|top| top.max_completion_tokens),
        })
        .collect())
}

/// Fetches `{base_url}/models`. No error contains `key`.
pub async fn fetch(
    base_url: &str,
    auth: Auth,
    key: &str,
    headers: Vec<(String, String)>,
) -> Result<Vec<Entry>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = ExtraHeaders::new(headers).apply(client().get(url));
    request = match auth {
        Auth::Bearer => request.bearer_auth(key),
        Auth::XApiKey => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
    };
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
