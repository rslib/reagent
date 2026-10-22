//! The `models` command: lists the models of providers, with a daily cache.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reagent_ai::catalog::{self, Auth, Cache, Entry, models_dev};

use crate::args::{ModelsArgs, Target, Wire};
use crate::run::{Source, api_key};

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

/// The catalogue URL, or `REAGENT_MODELS_DEV_URL` when it is set. That
/// variable is a test hook: the end-to-end tests point the catalogue at a fake
/// server, and nothing else reads it.
fn catalogue_url() -> String {
    std::env::var("REAGENT_MODELS_DEV_URL").unwrap_or_else(|_| models_dev::URL.to_string())
}

/// The models.dev provider id of a target and whether its prices are filled
/// too. The config's id wins over the preset's; a provider that is neither a
/// preset nor given an id is not filled.
fn fill_id(target: &Target) -> Option<(&str, bool)> {
    let prices = target.preset.is_none_or(|preset| preset.models_dev.prices);
    if let Some(id) = target.models_dev.as_deref() {
        return Some((id, prices));
    }
    let preset = target.preset?;
    Some((preset.models_dev.id.unwrap_or(preset.name), prices))
}

/// What the catalogue fills into one provider's fetched list.
struct Fill<'a> {
    /// The catalogue entries of the provider.
    entries: &'a [Entry],
    /// Whether a missing price is filled, or only the limits.
    prices: bool,
}

impl Fill<'_> {
    /// Fills what `entry` leaves out: its limits, and its price when the
    /// provider lists none and the catalogue's price counts.
    fn apply(&self, entry: &mut Entry) {
        let Some(source) = self.entries.iter().find(|source| source.id == entry.id) else {
            return;
        };
        if entry.context_window.is_none() {
            entry.context_window = source.context_window;
        }
        if entry.max_output.is_none() {
            entry.max_output = source.max_output;
        }
        if self.prices && entry.price.is_none() {
            entry.price = source.price;
            entry.price_source = source.price_source;
        }
    }
}

/// Loads the catalogue for `ids` once for the whole command. The crate has no
/// timer, so the deadline is here. A failure is one stderr line and no fill:
/// a list of models never fails because the catalogue did.
async fn catalogue(ids: &[&str], dir: Option<&PathBuf>) -> BTreeMap<String, Vec<Entry>> {
    let Some(dir) = dir.filter(|_| !ids.is_empty()) else {
        return BTreeMap::new();
    };
    let url = catalogue_url();
    let load = models_dev::load(dir, ids, now(), &url);
    match tokio::time::timeout(Duration::from_secs(30), load).await {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => {
            eprintln!("reagent: models.dev: {error}");
            BTreeMap::new()
        }
        Err(_) => {
            eprintln!("reagent: models.dev: no reply in 30 s");
            BTreeMap::new()
        }
    }
}

/// Whether listing `target` fetches: no cache, a stale cache, or `--refresh`.
fn needs_fetch(target: &Target, refresh: bool, dir: Option<&PathBuf>, now: u64) -> bool {
    if refresh {
        return true;
    }
    match dir.and_then(|dir| catalog::read(dir, &target.name)) {
        Some(cache) => !cache.is_fresh(now),
        None => true,
    }
}

/// The credential of `target` and where it came from: a saved login when it
/// has one, else its key.
async fn credential(target: &Target) -> Result<(reagent_ai::Credential, Source), String> {
    let named = target
        .api_key_env
        .as_deref()
        .and_then(|name| std::env::var(name).ok());
    // A saved ChatGPT login signs in with a token that refreshes; its own
    // header is the bearer the list request needs.
    let session = crate::run::saved_session(
        named.as_deref(),
        target.api_key.as_ref(),
        reagent_config::auth::dir().ok().as_deref(),
        Some(&target.login),
        reagent_ai::oauth::chatgpt::TOKEN,
        crate::run::sleeper(),
    )?;
    if let Some(session) = session {
        return Ok((
            reagent_ai::Credential::Session(session),
            Source::Login(target.login.clone()),
        ));
    }
    let (key, source) = api_key(
        target.api_key_env.as_deref(),
        target.api_key.as_ref(),
        Some(&target.login),
        &target.wire,
    )
    .map_err(|error| {
        crate::run::login_hint(
            target.preset,
            target.api_key_env.as_deref(),
            Some(&target.name),
            Some(&target.login),
            error,
        )
    })?;
    let credential = match target.wire {
        Wire::Anthropic => reagent_ai::Credential::x_api_key(&key)?,
        _ => reagent_ai::Credential::bearer(&key)?,
    };
    Ok((credential, source))
}

/// The line the command prints before it fetches `target`: the profile, the
/// provider and where its credential came from, never the key.
fn start_line(profile: Option<&str>, target: &Target, source: &Source) -> String {
    format!(
        "reagent: profile {}, provider {}, credential {}",
        profile.unwrap_or("none"),
        target.name,
        source.text()
    )
}

/// Fetches the list of `target`, fills it from the catalogue and writes the
/// cache.
async fn fetch(
    target: &Target,
    dir: Option<&PathBuf>,
    profile: Option<&str>,
    fill: Option<&Fill<'_>>,
) -> Result<Cache, String> {
    let auth = match &target.wire {
        Wire::Anthropic => Auth::XApiKey,
        Wire::OpenAiChat | Wire::OpenAiResponses => Auth::Bearer,
        #[cfg(feature = "testing")]
        Wire::Fake(_) => return Err("the scripted provider has no model list".into()),
    };
    let (credential, source) = credential(target).await?;
    eprintln!("{}", start_line(profile, target, &source));
    let base_url = target
        .base_url
        .clone()
        .unwrap_or_else(|| target.wire.base_url());
    let mut models = catalog::fetch(
        &base_url,
        target.models_path,
        auth,
        &credential,
        target.header.clone(),
    )
    .await?;
    if let Some(fill) = fill {
        for entry in &mut models {
            fill.apply(entry);
        }
    }
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
    profile: Option<&str>,
    fill: Option<&Fill<'_>>,
) -> Result<Vec<String>, String> {
    let cached = dir.and_then(|dir| catalog::read(dir, &target.name));
    let now = now();
    let cache = match cached {
        Some(cache) if !refresh && cache.is_fresh(now) => cache,
        cached => match fetch(target, dir, profile, fill).await {
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
    let start = now();
    let mut targets = Vec::new();
    for target in args.targets {
        // A provider that cannot sign in is not listed by name, and a built-in
        // one is dropped from a bare listing.
        let signed_in = credential(&target).await.is_ok();
        if target.implicit && !signed_in {
            continue;
        }
        targets.push((target, signed_in));
    }
    // One catalogue load serves the whole command, and only a provider that is
    // fetched and can sign in needs it: the others never reach the fill.
    let mut ids: Vec<&str> = Vec::new();
    for (target, signed_in) in &targets {
        let Some((id, _)) = fill_id(target) else {
            continue;
        };
        if *signed_in
            && needs_fetch(target, args.refresh, dir.as_ref(), start)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    let catalogue = catalogue(&ids, dir.as_ref()).await;
    let mut lines = Vec::new();
    let mut failed = false;
    for (target, _) in &targets {
        let fill = fill_id(target).map(|(id, prices)| Fill {
            entries: catalogue.get(id).map_or(&[][..], Vec::as_slice),
            prices,
        });
        let listed = list(
            target,
            args.refresh,
            dir.as_ref(),
            args.profile.as_deref(),
            fill.as_ref(),
        )
        .await;
        match listed {
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
