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
        // The provider's own list wins over the catalogue, as with the price.
        entry.fill_reasoning(source);
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

/// Whether `target` lists its models without a key. The radius preset reads
/// the endpoint's own `/config`, which is public.
fn lists_to_anyone(target: &Target) -> bool {
    target.preset.is_some_and(|preset| preset.public_list)
}

/// Whether the reason `target` had no credential is that no source held one:
/// a missing key variable, or the hint of a provider whose only credential is
/// a saved login.
///
/// Any other reason, a refused auth file, an expired login or a key the server
/// would not take as a header, is a real error and not a reason to list the
/// models without a credential.
fn no_credential(error: &str) -> bool {
    error.starts_with("missing ") || error.contains(" needs a login; run ")
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
        Wire::OpenAiChat | Wire::OpenAiResponses | Wire::PiMessages => Auth::Bearer,
        #[cfg(feature = "testing")]
        Wire::Fake(_) => return Err("the scripted provider has no model list".into()),
    };
    let credential = match credential(target).await {
        Ok((credential, source)) => {
            eprintln!("{}", start_line(profile, target, &source));
            Some(credential)
        }
        Err(error) if lists_to_anyone(target) && no_credential(&error) => {
            eprintln!(
                "reagent: {}: {error}; listing the public models",
                target.name
            );
            None
        }
        Err(error) => return Err(error),
    };
    let base_url = target
        .base_url
        .clone()
        .unwrap_or_else(|| target.wire.base_url());
    // The provider's own shape when it has one, else the wire's: only the
    // Anthropic wire lists its models differently from the OpenRouter shape.
    let read = target.preset.map_or_else(
        || match target.wire {
            Wire::Anthropic => reagent_ai::providers::anthropic::read_list,
            _ => catalog::list::read,
        },
        |preset| preset.read_list,
    );
    let mut models = catalog::fetch(
        &base_url,
        target.models_path,
        auth,
        credential.as_ref(),
        target.header.clone(),
        read,
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
        .map(|entry| line(target.name.as_str(), &entry))
        .collect())
}

/// One output line: `name/id`, then the model's reasoning levels when its plan
/// is known, such as `fi/m  off low high* max` or `fi/g  low high (no off)`.
fn line(name: &str, entry: &Entry) -> String {
    use std::fmt::Write as _;

    let mut out = format!("{name}/{}", entry.id);
    let Some(plan) = entry.reasoning.as_ref().filter(|plan| !plan.is_empty()) else {
        return out;
    };
    let start = out.len();
    let no_off = plan.off == Some(false) || plan.mandatory;
    if plan.off == Some(true) && !no_off {
        out.push_str(" off");
    }
    for level in plan.levels.iter() {
        let star = if plan.default == Some(level) { "*" } else { "" };
        let _ = write!(out, " {}{star}", level.name());
    }
    for word in &plan.unknown {
        out.push(' ');
        out.push_str(word);
    }
    if no_off {
        out.push_str(" (no off)");
    }
    // Two spaces part the id from the levels, so the first field stays the id.
    if out.len() > start {
        out.insert(start, ' ');
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use reagent_ai::levels::Reasoning;

    /// An entry for `id` with the reasoning plan `reasoning`.
    fn entry(id: &str, reasoning: Option<Reasoning>) -> Entry {
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
    fn levels_follow_the_id_with_off_the_default_and_unknown_words() {
        use reagent_ai::levels::Level;
        let full = Reasoning {
            off: Some(true),
            default: Some(Level::High),
            ..Reasoning::from_words(["low", "high", "max"])
        };
        assert_eq!(
            line("fi", &entry("m", Some(full))),
            "fi/m  off low high* max"
        );
        let strict = Reasoning {
            off: Some(false),
            ..Reasoning::from_words(["low", "ultra", "high"])
        };
        assert_eq!(
            line("fi", &entry("g", Some(strict))),
            "fi/g  low high ultra (no off)"
        );
        let mandatory = Reasoning {
            mandatory: true,
            ..Reasoning::from_words(["medium"])
        };
        assert_eq!(
            line("fi", &entry("t", Some(mandatory))),
            "fi/t  medium (no off)"
        );
        assert_eq!(line("fi", &entry("a", None)), "fi/a");
        assert_eq!(line("fi", &entry("e", Some(Reasoning::default()))), "fi/e");
    }

    #[test]
    fn reasoning_levels_the_catalogue_fills_only_a_missing_plan() {
        let catalogue = vec![entry("m", Some(Reasoning::from_words(["low", "high"])))];
        let fill = Fill {
            entries: &catalogue,
            prices: false,
        };
        // A provider list that names its own levels keeps them.
        let mut listed = entry("m", Some(Reasoning::from_words(["medium"])));
        fill.apply(&mut listed);
        assert_eq!(listed.reasoning, Some(Reasoning::from_words(["medium"])));
        // One that says nothing about reasoning is filled whatever the price
        // flag says.
        let mut blank = entry("m", None);
        fill.apply(&mut blank);
        assert_eq!(
            blank.reasoning,
            Some(Reasoning::from_words(["low", "high"]))
        );
        assert_eq!(blank.price, None, "the prices flag stays off");
        // A model the catalogue does not list gets nothing.
        let mut other = entry("n", None);
        fill.apply(&mut other);
        assert_eq!(other.reasoning, None);
    }
}
