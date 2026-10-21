//! The `models` command: lists the models of providers, with a daily cache.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use reagent_ai::catalog::{self, Auth, Cache};

use crate::args::{ModelsArgs, Target, Wire};
use crate::run::api_key;

/// The folder of the models cache: under `XDG_CACHE_HOME` when it is set,
/// else under `$HOME/.cache`.
pub fn cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(std::env::var_os("HOME").filter(|value| !value.is_empty())?)
            .join(".cache"),
    };
    Some(base.join("reagent").join("models"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn age(seconds: u64) -> String {
    match seconds {
        0..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// Fetches the list of `target` and writes the cache.
async fn fetch(target: &Target, dir: Option<&PathBuf>) -> Result<Cache, String> {
    let auth = match &target.wire {
        Wire::Anthropic => Auth::XApiKey,
        Wire::OpenAiChat | Wire::OpenAiResponses => Auth::Bearer,
        #[cfg(feature = "testing")]
        Wire::Fake(_) => return Err("the scripted provider has no model list".into()),
    };
    let key = api_key(
        target.api_key_env.as_deref(),
        target.api_key.as_ref(),
        &target.wire,
    )?;
    let base_url = target
        .base_url
        .clone()
        .unwrap_or_else(|| target.wire.base_url());
    let models = catalog::fetch(&base_url, auth, &key, target.header.clone()).await?;
    let cache = Cache {
        fetched_at: now(),
        models,
    };
    if let Some(dir) = dir
        && let Err(error) = catalog::write(dir, &target.name, &cache)
    {
        eprintln!("reagent: {}: cannot write the cache: {error}", target.name);
    }
    Ok(cache)
}

/// Lists one provider: the fresh cache, else a fetch, else the old cache.
async fn list(
    target: &Target,
    refresh: bool,
    dir: Option<&PathBuf>,
) -> Result<Vec<String>, String> {
    let cached = dir.and_then(|dir| catalog::read(dir, &target.name));
    let now = now();
    let cache = match cached {
        Some(cache) if !refresh && cache.is_fresh(now) => cache,
        cached => match fetch(target, dir).await {
            Ok(cache) => cache,
            Err(error) => {
                let Some(cache) = cached else {
                    return Err(error);
                };
                eprintln!(
                    "reagent: {}: {error}; using the cached list from {}",
                    target.name,
                    age(now.saturating_sub(cache.fetched_at))
                );
                cache
            }
        },
    };
    Ok(cache
        .models
        .into_iter()
        .map(|entry| format!("{}/{}", target.name, entry.id))
        .collect())
}

/// Runs the `models` command and returns the process exit code.
pub async fn run(args: ModelsArgs) -> ExitCode {
    let dir = cache_dir();
    let mut lines = Vec::new();
    let mut failed = false;
    for target in &args.targets {
        if target.implicit
            && api_key(
                target.api_key_env.as_deref(),
                target.api_key.as_ref(),
                &target.wire,
            )
            .is_err()
        {
            continue;
        }
        match list(target, args.refresh, dir.as_ref()).await {
            Ok(ids) => lines.extend(ids),
            Err(error) => {
                eprintln!("reagent: {}: {error}", target.name);
                failed = true;
            }
        }
    }
    lines.sort();
    for line in lines {
        println!("{line}");
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
