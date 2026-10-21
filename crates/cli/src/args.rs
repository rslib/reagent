//! Command line argument parsing with `lexopt`.

use std::path::{Path, PathBuf};

use reagent_config::permissions::Rules;
use reagent_config::{Layer, Model, Provider, Secret, Settings};

/// The default model for headless runs.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// The loop policy names a `--policy` value may select.
pub const POLICY_NAMES: [&str; 6] = [
    "completion",
    "stuck",
    "budget",
    "snapshot",
    "mask",
    "compact",
];

/// The policies `all` selects. `stuck` is left out until a benchmark run
/// shows it helps: a loop breaker cost 1.1 points in the Meta-Harness study.
const ALL_POLICIES: [&str; 3] = ["completion", "budget", "snapshot"];

/// The policies with no `--policy` flag. The budget note does nothing without
/// a deadline or a turn cap, and with one it cut timeouts on a slow model.
const DEFAULT_POLICIES: [&str; 1] = ["budget"];

/// The policies with no `--policy` flag on the chat wire. Masking cut prompt
/// tokens by 42% on a glm-5.3-flash dev run with no drop in pass rate.
const DEFAULT_CHAT_POLICIES: [&str; 2] = ["budget", "mask"];

/// The reply token cap when `--max-tokens` is absent. A `write` call carries
/// the whole file, and thinking counts toward the cap, so a low cap cuts the
/// reply.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// The provider wire selected by `--provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    /// The Anthropic Messages API.
    Anthropic,
    /// OpenAI-compatible Chat Completions.
    OpenAiChat,
    /// The OpenAI Responses API.
    OpenAiResponses,
    /// A scripted provider read from `fake:<file>`, testing builds only.
    #[cfg(feature = "testing")]
    Fake(String),
}

impl Wire {
    /// The environment variable holding the API key when `--api-key-env` is
    /// absent.
    pub fn default_api_key_env(&self) -> &'static str {
        match self {
            Wire::Anthropic => "ANTHROPIC_API_KEY",
            Wire::OpenAiChat | Wire::OpenAiResponses => "OPENAI_API_KEY",
            #[cfg(feature = "testing")]
            Wire::Fake(_) => "ANTHROPIC_API_KEY",
        }
    }

    /// The endpoint when `--base-url` is absent: the provider environment
    /// variable, else the built-in default.
    pub fn base_url(&self) -> String {
        let (variable, fallback) = match self {
            Wire::Anthropic => ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
            Wire::OpenAiChat | Wire::OpenAiResponses => {
                ("OPENAI_BASE_URL", "https://api.openai.com/v1")
            }
            #[cfg(feature = "testing")]
            Wire::Fake(_) => ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
        };
        std::env::var(variable)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    }
}

/// The parsed command line.
#[derive(Debug)]
pub struct Args {
    /// The task the agent should complete, or where to read it from.
    pub task: TaskSource,
    /// Print one JSON object per event.
    pub json: bool,
    /// The provider model id.
    pub model: String,
    /// The provider wire to use.
    pub provider: Wire,
    /// The endpoint URL, or the provider default when absent.
    pub base_url: Option<String>,
    /// The name of the environment variable holding the API key, or the
    /// provider default when absent.
    pub api_key_env: Option<String>,
    /// The API key from the provider table of the config, if any.
    pub api_key: Option<Secret>,
    /// The config or built-in provider name from `--model name/id`, if any.
    pub provider_name: Option<String>,
    /// Extra request headers from `--header`, in order.
    pub header: Vec<(String, String)>,
    /// The cap on reply tokens, thinking included.
    pub max_tokens: u32,
    /// A hard deadline in seconds, if any.
    pub deadline: Option<u64>,
    /// The working directory the tools run in.
    pub cwd: PathBuf,
    /// The loop policies to enable, expanded and de-duplicated.
    pub policy: Vec<String>,
    /// Folder names from the config that searches leave out.
    pub skip: Vec<String>,
    /// The permission rules of the config files.
    pub rules: Rules,
    /// The context window of the model in tokens, from its config table.
    pub context_window: Option<u64>,
    /// How bash commands are fenced in.
    pub sandbox: SandboxMode,
    /// Whether sandboxed commands may use the network.
    pub network: bool,
    /// Folders sandboxed commands may write, besides the working and temp
    /// folders.
    pub write: Vec<PathBuf>,
}

/// One provider the `models` command lists.
#[derive(Debug)]
pub struct Target {
    /// The provider name, as in `--model name/id`.
    pub name: String,
    /// The provider wire.
    pub wire: Wire,
    /// The endpoint URL, or the wire default when absent.
    pub base_url: Option<String>,
    /// The environment variable holding the API key, or the wire default.
    pub api_key_env: Option<String>,
    /// The API key from the provider table of the config, if any.
    pub api_key: Option<Secret>,
    /// Extra request headers from the provider table.
    pub header: Vec<(String, String)>,
    /// A built-in provider with no config table, listed with no name only when
    /// it has a key.
    pub implicit: bool,
}

/// The `models` command.
#[derive(Debug)]
pub struct ModelsArgs {
    /// The providers to list.
    pub targets: Vec<Target>,
    /// Fetch even when the cache is fresh.
    pub refresh: bool,
}

/// The `sandbox.mode` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Run bash directly.
    Off,
    /// Sandbox bash when the sandbox can start, else run it directly.
    Auto,
    /// Sandbox bash or refuse to run it.
    On,
}

/// Reads the sandbox settings, expanding a leading `~/` in the write folders
/// with `home`.
fn resolve_sandbox(
    sandbox: &reagent_config::Sandbox,
    home: Option<&Path>,
) -> Result<(SandboxMode, bool, Vec<PathBuf>), String> {
    let mode = match sandbox.mode.as_deref() {
        None | Some("off") => SandboxMode::Off,
        Some("auto") => SandboxMode::Auto,
        Some("on") => SandboxMode::On,
        Some(other) => {
            return Err(format!(
                "config: sandbox.mode must be off, auto or on, not \"{other}\""
            ));
        }
    };
    let write = sandbox
        .write
        .iter()
        .map(|dir| match (dir.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(dir),
        })
        .collect();
    Ok((mode, sandbox.network.unwrap_or(false), write))
}

/// Where the task text comes from.
#[derive(Debug)]
pub enum TaskSource {
    /// The task was given on the command line.
    Text(String),
    /// The task is read from standard input (`-p -`).
    Stdin,
}

/// Either the parsed arguments or an immediate exit decision.
#[derive(Debug)]
pub enum Outcome {
    /// Run the agent with these arguments.
    Run(Box<Args>),
    /// List the models of the providers.
    Models(Box<ModelsArgs>),
    /// Print the help text and exit cleanly.
    Help,
    /// Print the version and exit cleanly.
    Version,
}

fn usage() -> &'static str {
    concat!(
        "usage: reagent [OPTIONS] -p <task>\n",
        "\n",
        "Runs the agent headless (no terminal UI) and prints the result.\n",
        "\n",
        "Options:\n",
        "  -p, --print <task>   the task for the agent to complete; `-` reads it from stdin\n",
        "      --json           print one JSON object per event on stdout\n",
        "      --model <id>     the provider model id\n",
        "      --provider <wire>  anthropic, openai-chat or openai-responses (default: anthropic)\n",
        "      --base-url <url>   the endpoint; default from the provider env, else built in\n",
        "      --api-key-env <name>  the env var holding the API key; never the key itself\n",
        "      --header <name:value>  an extra request header; repeatable; reserved names\n",
        "                       (authorization, x-api-key, content-type, anthropic-version) are errors\n",
        "      --max-tokens <n> the reply token cap, thinking included (default: 64000)\n",
        "      --deadline <s>   stop after this many seconds\n",
        "      --cwd <dir>      the working directory for the tools (default: current dir)\n",
        "      --policy <list>  loop policies: completion, stuck, budget, snapshot, mask, compact;\n",
        "                       all = completion, budget, snapshot; none = off\n",
        "                       (default: budget, plus mask on openai-chat)\n",
        "      --version        print the version and exit\n",
        "\n",
        "Commands:\n",
        "  models [name] [--refresh]  list the models of a provider, or of all with a key\n"
    )
}

#[cfg(feature = "testing")]
fn extra_usage() -> &'static str {
    "      --provider fake:<file>  testing only: play a JSON script of replies\n"
}

#[cfg(not(feature = "testing"))]
fn extra_usage() -> &'static str {
    ""
}

fn bad(field: &str) -> String {
    format!("invalid value for {field}")
}

/// Whether the wire replays no reasoning that depends on earlier messages,
/// so compaction may rewrite them. The test-only scripted wire replays none.
fn may_compact(provider: &Wire) -> bool {
    match provider {
        Wire::OpenAiChat => true,
        #[cfg(feature = "testing")]
        Wire::Fake(_) => true,
        _ => false,
    }
}

/// Maps a `--provider` value to a wire, or to `fake:<file>` under testing.
fn parse_provider(value: &str) -> Result<Wire, String> {
    match value {
        "anthropic" => Ok(Wire::Anthropic),
        "openai-chat" => Ok(Wire::OpenAiChat),
        "openai-responses" => Ok(Wire::OpenAiResponses),
        other => {
            #[cfg(feature = "testing")]
            if let Some(spec) = other.strip_prefix("fake:") {
                return Ok(Wire::Fake(spec.to_string()));
            }
            Err(format!("unknown provider: {other}"))
        }
    }
}

/// Splits a `--header` value at its first colon and checks it.
///
/// No error contains the argument or the value: a key pasted into a header
/// must not reach an error message.
fn parse_header(text: &str) -> Result<(String, String), String> {
    let Some((name, value)) = text.split_once(':') else {
        return Err("invalid header, expected name:value".to_string());
    };
    let name = name.trim();
    if name.is_empty() {
        return Err("invalid header: the name is empty".to_string());
    }
    let value = value.trim();
    reagent_ai::check_header(name, value)?;
    Ok((name.to_string(), value.to_string()))
}

/// Expands and validates a `--policy` comma list into sorted, de-duplicated
/// policy names. `all` expands to [`ALL_POLICIES`] and `none`, valid only
/// alone, to no policy. An unknown name is an error, which the binary reports
/// by exiting 2.
pub fn parse_policy(value: &str) -> Result<Vec<String>, String> {
    if value.trim() == "none" {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match part {
            "none" => return Err("none must be the only policy".to_string()),
            "all" => names.extend(ALL_POLICIES.iter().map(ToString::to_string)),
            name if POLICY_NAMES.contains(&name) => names.push(name.to_string()),
            other => return Err(format!("unknown policy: {other}")),
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Parses the process arguments, or returns an exit decision.
pub fn parse() -> Result<Outcome, String> {
    parse_from(lexopt::Parser::from_env(), &load_settings)
}

/// Replaces the policy names of `layer` with their expanded form, naming the
/// file when one is wrong.
fn expand_policies(layer: &mut Layer) -> Result<(), String> {
    let path = layer.path.display().to_string();
    let expand = |names: &mut Option<Vec<String>>| -> Result<(), String> {
        if let Some(list) = names {
            *list = parse_policy(&list.join(",")).map_err(|error| format!("{path}: {error}"))?;
        }
        Ok(())
    };
    expand(&mut layer.settings.policy)?;
    for provider in layer.settings.providers.values_mut() {
        for model in provider.models.values_mut() {
            expand(&mut model.policy)?;
        }
    }
    Ok(())
}

/// Loads the config files for `cwd`, checks their policy names and merges
/// them, and parses their permission rules.
fn load_settings(cwd: &Path) -> Result<(Settings, Rules), String> {
    settings_from(reagent_config::load_for(cwd)?, cwd)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the config is read before the runtime starts"
)]
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn settings_from(mut layers: Vec<Layer>, cwd: &Path) -> Result<(Settings, Rules), String> {
    for layer in &mut layers {
        expand_policies(layer)?;
    }
    // The tools resolve paths from the real folder, so the rules must too,
    // or a symlinked --cwd would make every relative path rule miss.
    let cwd = canonical(cwd);
    let home = std::env::var_os("HOME").map(|home| canonical(Path::new(&home)));
    let rules = Rules::from_layers(&layers, &cwd, home.as_deref())?;
    Ok((reagent_config::merge(&layers), rules))
}

/// The provider a name selects when no config table defines it.
fn built_in_provider(name: &str) -> Option<(Wire, &'static str)> {
    match name {
        "anthropic" => Some((Wire::Anthropic, "ANTHROPIC_API_KEY")),
        "openai" => Some((Wire::OpenAiResponses, "OPENAI_API_KEY")),
        _ => None,
    }
}

/// Splits the model text at its first `/` when the part before it names a
/// built-in or configured provider.
fn split_model<'a>(text: &'a str, settings: &Settings) -> Option<(&'a str, &'a str)> {
    let (name, id) = text.split_once('/')?;
    (settings.providers.contains_key(name) || built_in_provider(name).is_some())
        .then_some((name, id))
}

/// The providers `models` lists: the one `name` selects, else every config
/// table and every built-in provider.
fn model_targets(settings: &Settings, name: Option<&str>) -> Result<Vec<Target>, String> {
    let mut names: Vec<&str> = settings.providers.keys().map(String::as_str).collect();
    for built_in in ["anthropic", "openai"] {
        if !names.contains(&built_in) {
            names.push(built_in);
        }
    }
    if let Some(name) = name {
        let Some(chosen) = names.iter().copied().find(|known| *known == name) else {
            return Err(format!("unknown provider: {name}"));
        };
        names = vec![chosen];
    }
    let name_given = name;
    names
        .into_iter()
        .map(|name| {
            let table = settings.providers.get(name).cloned().unwrap_or_default();
            let built_in = built_in_provider(name);
            let wire = match (&table.wire, &built_in) {
                (Some(wire), _) => {
                    parse_provider(wire).map_err(|error| format!("config: {error}"))?
                }
                (None, Some((wire, _))) => wire.clone(),
                (None, None) => Wire::Anthropic,
            };
            let mut header = Vec::new();
            for (key, value) in &table.headers {
                reagent_ai::check_header(key, value).map_err(|error| format!("config: {error}"))?;
                header.push((key.clone(), value.clone()));
            }
            Ok(Target {
                name: name.to_string(),
                wire,
                base_url: table.base_url,
                api_key_env: table
                    .api_key_env
                    .or_else(|| built_in.map(|(_, key)| key.to_string())),
                api_key: table.api_key,
                header,
                implicit: name_given.is_none() && !settings.providers.contains_key(name),
            })
        })
        .collect()
}

/// Reads the merged settings and the permission rules for a working folder.
type LoadSettings<'a> = dyn Fn(&Path) -> Result<(Settings, Rules), String> + 'a;

/// Parses `args` (the arguments after the program name) into an outcome.
/// `load` reads the settings for the working directory; a flag beats them.
fn parse_from(mut parser: lexopt::Parser, load: &LoadSettings) -> Result<Outcome, String> {
    let mut task: Option<TaskSource> = None;
    let mut json = false;
    let mut model = None;
    let mut provider = None;
    let mut base_url = None;
    let mut api_key_env = None;
    let mut header = Vec::new();
    let mut max_tokens = None;
    let mut deadline = None;
    let mut cwd = None;
    let mut policy: Option<Vec<String>> = None;
    let mut models: Option<Option<String>> = None;
    let mut refresh = false;
    let mut first = true;

    while let Some(arg) = parser.next().map_err(|error| error.to_string())? {
        let at_start = std::mem::take(&mut first);
        match arg {
            lexopt::Arg::Short('p') | lexopt::Arg::Long("print") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --print".to_string())?;
                let text = if value == "-" {
                    TaskSource::Stdin
                } else {
                    TaskSource::Text(
                        value
                            .into_string()
                            .map_err(|_| "task must be UTF-8".to_string())?,
                    )
                };
                task = Some(text);
            }
            lexopt::Arg::Long("json") => json = true,
            lexopt::Arg::Long("model") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --model".to_string())?;
                model = Some(value.into_string().map_err(|_| bad("--model"))?);
            }
            lexopt::Arg::Long("provider") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --provider".to_string())?;
                let text = value.into_string().map_err(|_| bad("--provider"))?;
                provider = Some(parse_provider(&text)?);
            }
            lexopt::Arg::Long("base-url") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --base-url".to_string())?;
                base_url = Some(value.into_string().map_err(|_| bad("--base-url"))?);
            }
            lexopt::Arg::Long("api-key-env") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --api-key-env".to_string())?;
                api_key_env = Some(value.into_string().map_err(|_| bad("--api-key-env"))?);
            }
            lexopt::Arg::Long("header") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --header".to_string())?;
                let text = value.into_string().map_err(|_| bad("--header"))?;
                header.push(parse_header(&text)?);
            }
            lexopt::Arg::Long("max-tokens") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --max-tokens".to_string())?;
                let text = value.into_string().map_err(|_| bad("--max-tokens"))?;
                max_tokens = Some(
                    text.parse()
                        .ok()
                        .filter(|cap| *cap > 0)
                        .ok_or_else(|| bad("--max-tokens"))?,
                );
            }
            lexopt::Arg::Long("deadline") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --deadline".to_string())?;
                let text = value.into_string().map_err(|_| bad("--deadline"))?;
                deadline = Some(text.parse().map_err(|_| bad("--deadline"))?);
            }
            lexopt::Arg::Long("policy") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --policy".to_string())?;
                let text = value.into_string().map_err(|_| bad("--policy"))?;
                let names = policy.get_or_insert_with(Vec::new);
                names.extend(parse_policy(&text)?);
                names.sort();
                names.dedup();
            }
            lexopt::Arg::Long("cwd") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --cwd".to_string())?;
                cwd = Some(PathBuf::from(value));
            }
            lexopt::Arg::Long("refresh") => refresh = true,
            lexopt::Arg::Value(value) if models.is_some() || (at_start && value == "models") => {
                let text = value.into_string().map_err(|_| bad("argument"))?;
                match &mut models {
                    None => models = Some(None),
                    Some(name @ None) => *name = Some(text),
                    Some(Some(_)) => return Err(format!("unexpected argument: {text}")),
                }
            }
            lexopt::Arg::Long("help") => return Ok(Outcome::Help),
            lexopt::Arg::Short('h') => return Ok(Outcome::Help),
            lexopt::Arg::Long("version") => return Ok(Outcome::Version),
            _ => return Err(format!("unknown argument: {arg:?}")),
        }
    }

    if let Some(name) = models {
        if task.is_some()
            || json
            || model.is_some()
            || provider.is_some()
            || base_url.is_some()
            || api_key_env.is_some()
            || !header.is_empty()
            || max_tokens.is_some()
            || deadline.is_some()
            || policy.is_some()
        {
            return Err("the models command takes no run flags".into());
        }
        let (settings, _) = load(&cwd.unwrap_or_else(|| PathBuf::from(".")))?;
        let targets = model_targets(&settings, name.as_deref())?;
        return Ok(Outcome::Models(Box::new(ModelsArgs { targets, refresh })));
    }
    if refresh {
        return Err("--refresh needs the models command".into());
    }
    let Some(task) = task else {
        if json {
            return Err("the --json flag needs a task; use -p <task>".into());
        }
        return Err("missing task; use -p <task>".into());
    };
    let cwd = cwd.unwrap_or_else(|| PathBuf::from("."));
    let (settings, rules) = load(&cwd)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (sandbox, network, write) = resolve_sandbox(&settings.sandbox, home.as_deref())?;
    let model_text = model
        .or_else(|| settings.model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.into());
    let (provider_name, model_id) = match split_model(&model_text, &settings) {
        Some((name, id)) => (Some(name), id.to_string()),
        None => (None, model_text.clone()),
    };
    let table = provider_name.and_then(|name| settings.providers.get(name));
    let built_in = provider_name.and_then(built_in_provider);
    let table = table.cloned().unwrap_or_else(Provider::default);
    let model_table = table
        .models
        .get(&model_id)
        .cloned()
        .unwrap_or_else(Model::default);
    let provider = match (provider, &table.wire) {
        (Some(wire), _) => wire,
        (None, Some(name)) => parse_provider(name).map_err(|error| format!("config: {error}"))?,
        (None, None) => built_in.clone().map_or(Wire::Anthropic, |(wire, _)| wire),
    };
    let mut config_header = Vec::new();
    for (name, value) in &table.headers {
        reagent_ai::check_header(name, value).map_err(|error| format!("config: {error}"))?;
        config_header.push((name.clone(), value.clone()));
    }
    config_header.extend(header);
    let header = config_header;
    let policy = policy
        .or(model_table.policy)
        .or(settings.policy)
        .unwrap_or_else(|| {
            let names: &[&str] = if provider == Wire::OpenAiChat {
                &DEFAULT_CHAT_POLICIES
            } else {
                &DEFAULT_POLICIES
            };
            names.iter().map(ToString::to_string).collect()
        });
    // Editing old tool results breaks the reasoning the Anthropic and
    // Responses wires replay, so masking runs only on the chat wire.
    if policy.iter().any(|name| name == "mask") && provider != Wire::OpenAiChat {
        return Err("--policy mask needs --provider openai-chat".into());
    }
    if policy.iter().any(|name| name == "compact") && !may_compact(&provider) {
        return Err("--policy compact needs --provider openai-chat".into());
    }

    Ok(Outcome::Run(Box::new(Args {
        task,
        json,
        model: model_id,
        provider,
        base_url: base_url.or(table.base_url),
        api_key: table.api_key,
        provider_name: provider_name.map(str::to_string),
        api_key_env: api_key_env
            .or(table.api_key_env)
            .or_else(|| built_in.map(|(_, key)| key.to_string())),
        header,
        max_tokens: max_tokens
            .or(model_table.max_tokens)
            .or(settings.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS),
        deadline,
        cwd,
        policy,
        skip: settings.skip,
        rules,
        context_window: model_table.context_window,
        sandbox,
        network,
        write,
    })))
}

/// The help text, with testing-only flags when built for tests.
pub fn help() -> String {
    let mut help = usage().to_string();
    help.push_str(extra_usage());
    help
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Outcome, String> {
        parse_with(args, Settings::default())
    }

    fn parse_with(args: &[&str], settings: Settings) -> Result<Outcome, String> {
        let argv = args.iter().map(std::ffi::OsString::from);
        parse_from(lexopt::Parser::from_args(argv), &|_| {
            Ok((settings.clone(), Rules::none()))
        })
    }

    fn run_with(args: &[&str], settings: Settings) -> Args {
        match parse_with(args, settings) {
            Ok(Outcome::Run(args)) => *args,
            other => panic!("expected a run outcome for {args:?}, got {other:?}"),
        }
    }

    fn sandbox_table(mode: Option<&str>, write: &[&str]) -> reagent_config::Sandbox {
        reagent_config::Sandbox {
            mode: mode.map(str::to_string),
            network: Some(true),
            write: write.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn sandbox_mode_is_off_auto_or_on() {
        let home = Some(Path::new("/home/u"));
        for (text, mode) in [
            (None, SandboxMode::Off),
            (Some("off"), SandboxMode::Off),
            (Some("auto"), SandboxMode::Auto),
            (Some("on"), SandboxMode::On),
        ] {
            let (got, network, _) =
                resolve_sandbox(&sandbox_table(text, &[]), home).expect("resolves");
            assert_eq!((got, network), (mode, true));
        }
        let error = resolve_sandbox(&sandbox_table(Some("maybe"), &[]), home).unwrap_err();
        assert_eq!(
            error,
            "config: sandbox.mode must be off, auto or on, not \"maybe\""
        );
    }

    #[test]
    fn sandbox_write_folders_expand_a_leading_home() {
        let table = sandbox_table(Some("on"), &["~/.cache", "/data", "rel"]);
        let (_, _, write) = resolve_sandbox(&table, Some(Path::new("/home/u"))).expect("resolves");
        assert_eq!(
            write,
            [
                PathBuf::from("/home/u/.cache"),
                PathBuf::from("/data"),
                PathBuf::from("rel")
            ]
        );
    }

    #[test]
    fn sandbox_settings_reach_the_args() {
        let settings = Settings {
            sandbox: sandbox_table(Some("auto"), &["/data"]),
            ..Settings::default()
        };
        let args = run_with(&["-p", "x"], settings);
        assert_eq!(args.sandbox, SandboxMode::Auto);
        assert!(args.network);
        assert_eq!(args.write, [PathBuf::from("/data")]);
    }

    fn providers(provider: Provider) -> std::collections::BTreeMap<String, Provider> {
        [("fi".to_string(), provider)].into()
    }

    fn run(args: &[&str]) -> Args {
        match parse(args) {
            Ok(Outcome::Run(args)) => *args,
            other => panic!("expected a run outcome for {args:?}, got {other:?}"),
        }
    }

    #[test]
    fn policy_default_is_budget_only() {
        assert_eq!(run(&["-p", "task"]).policy, vec!["budget"]);
    }

    #[test]
    fn policy_default_is_replaced_by_a_flag() {
        assert_eq!(
            run(&["-p", "task", "--policy", "stuck"]).policy,
            vec!["stuck"]
        );
    }

    #[test]
    fn policy_default_on_the_chat_wire_is_budget_and_mask() {
        let args = run(&["-p", "task", "--provider", "openai-chat"]);
        assert_eq!(args.policy, vec!["budget", "mask"]);
    }

    #[test]
    fn policy_flag_on_the_chat_wire_replaces_the_default() {
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "stuck",
        ]);
        assert_eq!(args.policy, vec!["stuck"]);
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "none",
        ]);
        assert!(args.policy.is_empty());
    }

    #[test]
    fn policy_default_is_turned_off_by_none() {
        assert!(run(&["-p", "task", "--policy", "none"]).policy.is_empty());
    }

    #[test]
    fn policy_default_none_with_a_name_is_an_error() {
        assert!(parse(&["-p", "task", "--policy", "none,budget"]).is_err());
    }

    #[test]
    fn repeated_policy_flags_add_up() {
        let args = run(&[
            "-p",
            "task",
            "--policy",
            "stuck",
            "--policy",
            "budget,stuck",
        ]);
        assert_eq!(args.policy, vec!["budget", "stuck"]);
    }

    #[test]
    fn a_single_policy_is_selected() {
        let args = run(&["-p", "task", "--policy", "stuck"]);
        assert_eq!(args.policy, vec!["stuck"]);
    }

    #[test]
    fn all_expands_to_every_policy_but_stuck_sorted() {
        let args = run(&["-p", "task", "--policy", "all"]);
        assert_eq!(args.policy, vec!["budget", "completion", "snapshot"]);
        let args = run(&["-p", "task", "--policy", "all,stuck"]);
        assert_eq!(
            args.policy,
            vec!["budget", "completion", "snapshot", "stuck"]
        );
    }

    #[test]
    fn a_comma_list_is_sorted_and_de_duplicated() {
        let args = run(&["-p", "task", "--policy", "budget,completion,budget"]);
        assert_eq!(args.policy, vec!["budget", "completion"]);
    }

    #[test]
    fn can_mix_all_with_a_name() {
        let args = run(&["-p", "task", "--policy", "all,snapshot"]);
        assert_eq!(args.policy, vec!["budget", "completion", "snapshot"]);
    }

    #[test]
    fn policy_mask_needs_the_chat_wire() {
        assert!(parse(&["-p", "task", "--policy", "mask"]).is_err());
        assert!(
            parse(&[
                "-p",
                "task",
                "--policy",
                "mask",
                "--provider",
                "openai-responses"
            ])
            .is_err()
        );
    }

    #[test]
    fn compact_policy_needs_the_chat_wire() {
        let error = parse(&["-p", "task", "--policy", "compact"]).unwrap_err();
        assert!(error.contains("openai-chat"), "{error}");
        let chat = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "compact",
        ]);
        assert_eq!(chat.policy, vec!["compact"]);
        for wire in ["anthropic", "openai-responses"] {
            let refused = parse(&["-p", "task", "--policy", "compact", "--provider", wire]);
            assert!(refused.is_err(), "{wire}");
        }
    }

    #[test]
    fn policy_mask_runs_on_the_chat_wire() {
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "budget,mask",
        ]);
        assert_eq!(args.policy, vec!["budget", "mask"]);
    }

    #[test]
    fn policy_mask_is_not_in_all() {
        let args = run(&["-p", "task", "--provider", "openai-chat", "--policy", "all"]);
        assert!(!args.policy.iter().any(|name| name == "mask"));
    }

    #[test]
    fn an_unknown_policy_is_an_error() {
        assert!(parse(&["-p", "task", "--policy", "nope"]).is_err());
    }

    #[test]
    fn a_missing_policy_value_is_an_error() {
        assert!(parse(&["-p", "task", "--policy"]).is_err());
    }

    #[test]
    fn the_default_provider_is_anthropic() {
        let args = run(&["-p", "task"]);
        assert_eq!(args.provider, Wire::Anthropic);
        assert_eq!(args.base_url, None);
        assert_eq!(args.api_key_env, None);
        assert!(args.header.is_empty());
    }

    #[test]
    fn each_provider_name_selects_its_wire() {
        assert_eq!(
            run(&["-p", "task", "--provider", "anthropic"]).provider,
            Wire::Anthropic
        );
        assert_eq!(
            run(&["-p", "task", "--provider", "openai-chat"]).provider,
            Wire::OpenAiChat
        );
        assert_eq!(
            run(&["-p", "task", "--provider", "openai-responses"]).provider,
            Wire::OpenAiResponses
        );
    }

    #[test]
    fn an_unknown_provider_is_an_error() {
        assert!(parse(&["-p", "task", "--provider", "google"]).is_err());
    }

    #[test]
    fn the_openai_api_key_env_default_differs_from_anthropic() {
        assert_eq!(Wire::Anthropic.default_api_key_env(), "ANTHROPIC_API_KEY");
        assert_eq!(Wire::OpenAiChat.default_api_key_env(), "OPENAI_API_KEY");
        assert_eq!(
            Wire::OpenAiResponses.default_api_key_env(),
            "OPENAI_API_KEY"
        );
    }

    #[test]
    fn a_base_url_flag_overrides_the_default() {
        let args = run(&["-p", "task", "--base-url", "http://localhost:8080"]);
        assert_eq!(args.base_url.as_deref(), Some("http://localhost:8080"));
    }

    #[test]
    fn an_api_key_env_flag_is_remembered() {
        let args = run(&["-p", "task", "--api-key-env", "MY_KEY"]);
        assert_eq!(args.api_key_env.as_deref(), Some("MY_KEY"));
    }

    #[test]
    fn a_header_is_split_at_the_first_colon() {
        let args = run(&["-p", "task", "--header", "x-opencode-session:abc:def"]);
        assert_eq!(
            args.header,
            vec![("x-opencode-session".to_string(), "abc:def".to_string())]
        );
    }

    #[test]
    fn repeated_headers_are_kept_in_order() {
        let args = run(&["-p", "task", "--header", "a:1", "--header", "b:2"]);
        assert_eq!(
            args.header,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn a_header_without_a_colon_is_an_error() {
        let error = parse(&["-p", "task", "--header", "sk-secret"]).unwrap_err();
        assert!(error.contains("name:value"), "{error}");
        assert!(!error.contains("sk-secret"), "{error}");
    }

    #[test]
    fn a_reserved_or_invalid_header_is_an_error() {
        for header in ["authorization:sk-secret", "x api:sk-secret"] {
            let error = parse(&["-p", "task", "--header", header]).unwrap_err();
            assert!(!error.contains("sk-secret"), "{error}");
        }
    }

    #[test]
    fn an_empty_header_name_is_an_error() {
        let error = parse(&["-p", "task", "--header", ":value"]).unwrap_err();
        assert!(error.contains("name"), "{error}");
    }

    #[cfg(feature = "testing")]
    #[test]
    fn a_fake_provider_is_parsed_under_testing() {
        let args = run(&["-p", "task", "--provider", "fake:replies.json"]);
        assert_eq!(args.provider, Wire::Fake("replies.json".to_string()));
    }

    #[test]
    fn max_tokens_flag_defaults_to_64000() {
        assert_eq!(run(&["-p", "task"]).max_tokens, 64_000);
    }

    #[test]
    fn max_tokens_flag_sets_the_cap() {
        assert_eq!(
            run(&["-p", "task", "--max-tokens", "16000"]).max_tokens,
            16_000
        );
    }

    #[test]
    fn max_tokens_flag_rejects_a_missing_zero_or_bad_value() {
        assert!(parse(&["-p", "task", "--max-tokens"]).is_err());
        assert!(parse(&["-p", "task", "--max-tokens", "0"]).is_err());
        assert!(parse(&["-p", "task", "--max-tokens", "many"]).is_err());
    }

    #[test]
    fn flags_beat_the_config() {
        let settings = Settings {
            model: Some("glm".into()),
            max_tokens: Some(1000),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task"], settings.clone());
        assert_eq!(args.model, "glm");
        assert_eq!(args.max_tokens, 1000);
        let args = run_with(
            &["-p", "task", "--model", "qwen", "--max-tokens", "2000"],
            settings,
        );
        assert_eq!(args.model, "qwen");
        assert_eq!(args.max_tokens, 2000);
    }

    #[test]
    fn a_config_policy_replaces_the_default() {
        let settings = Settings {
            model: Some("fi/glm".into()),
            policy: Some(vec!["stuck".into()]),
            skip: vec!["data".into()],
            providers: providers(Provider {
                wire: Some("openai-chat".into()),
                ..Provider::default()
            }),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task"], settings.clone());
        assert_eq!(args.provider, Wire::OpenAiChat);
        assert_eq!(args.policy, vec!["stuck"]);
        assert_eq!(args.skip, vec!["data"]);
        let args = run_with(&["-p", "task", "--policy", "budget"], settings);
        assert_eq!(args.policy, vec!["budget"]);
    }

    #[test]
    fn a_bad_config_policy_name_names_the_file() {
        let model = reagent_config::Model {
            policy: Some(vec!["nope".into()]),
            ..reagent_config::Model::default()
        };
        let mut provider = Provider::default();
        provider.models.insert("glm".into(), model);
        let mut layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/home/u/.config/reagent/config.toml"),
            settings: Settings {
                providers: providers(provider),
                ..Settings::default()
            },
        };
        let error = expand_policies(&mut layer).unwrap_err();
        assert!(
            error.starts_with("/home/u/.config/reagent/config.toml: "),
            "{error}"
        );
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn config_policy_names_are_expanded() {
        let mut layer = reagent_config::Layer {
            path: std::path::PathBuf::from("config.toml"),
            settings: Settings {
                policy: Some(vec!["all".into()]),
                ..Settings::default()
            },
        };
        expand_policies(&mut layer).unwrap();
        assert_eq!(
            layer.settings.policy,
            Some(vec![
                "budget".into(),
                "completion".into(),
                "snapshot".into()
            ])
        );
    }

    #[test]
    fn permissions_come_from_the_config() {
        let layer = Layer {
            path: std::path::PathBuf::from("/g/config.toml"),
            settings: Settings {
                permissions: reagent_config::permissions::Lists {
                    deny: vec!["Bash(git push:*)".into()],
                    ..Default::default()
                },
                ..Settings::default()
            },
        };
        let (_, rules) = settings_from(vec![layer], Path::new("/work")).unwrap();
        assert!(!rules.is_open());
        let bad = Layer {
            path: std::path::PathBuf::from("/g/config.toml"),
            settings: Settings {
                permissions: reagent_config::permissions::Lists {
                    deny: vec!["Bash(".into()],
                    ..Default::default()
                },
                ..Settings::default()
            },
        };
        let error = settings_from(vec![bad], Path::new("/work")).unwrap_err();
        assert!(error.starts_with("/g/config.toml"), "{error}");
    }

    #[test]
    fn a_config_error_ends_the_parse() {
        let argv = ["-p", "task"].iter().map(std::ffi::OsString::from);
        let error = parse_from(lexopt::Parser::from_args(argv), &|_| Err("bad".into()));
        assert_eq!(error.unwrap_err(), "bad");
    }

    #[test]
    fn models_command_takes_a_name_and_refresh() {
        let settings = Settings {
            providers: providers(Provider::default()),
            ..Settings::default()
        };
        let parsed = parse_with(&["models", "fi", "--refresh"], settings.clone());
        let Ok(Outcome::Models(models)) = parsed else {
            panic!("expected a models outcome, got {parsed:?}");
        };
        assert!(models.refresh);
        assert_eq!(models.targets.len(), 1);
        assert_eq!(models.targets[0].name, "fi");
        assert!(!models.targets[0].implicit);
        let parsed = parse_with(&["models"], settings.clone());
        let Ok(Outcome::Models(models)) = parsed else {
            panic!("expected a models outcome, got {parsed:?}");
        };
        let names: Vec<&str> = models.targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["fi", "anthropic", "openai"]);
        assert!(parse_with(&["models", "nope"], settings.clone()).is_err());
        assert!(parse_with(&["--refresh", "-p", "x"], settings.clone()).is_err());
        assert!(parse_with(&["models", "-p", "x"], settings.clone()).is_err());
        assert!(parse_with(&["models", "--model", "fi/m"], settings.clone()).is_err());
        assert!(parse_with(&["-p", "x", "models"], settings).is_err());
    }

    #[test]
    fn args_debug_hides_the_api_key() {
        let settings = Settings {
            providers: [(
                "fi".to_string(),
                Provider {
                    api_key: Some("sk-test-123".into()),
                    ..Provider::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task", "--model", "fi/m"], settings);
        assert!(args.api_key.is_some());
        let text = format!("{args:?}");
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("sk-test-123"), "{text}");
    }

    #[test]
    fn a_provider_prefix_picks_the_provider() {
        let settings = Settings {
            providers: [(
                "freeinference".to_string(),
                Provider {
                    wire: Some("openai-chat".into()),
                    base_url: Some("https://fi.example/v1".into()),
                    api_key_env: Some("FI_KEY".into()),
                    headers: [("x-team".to_string(), "a".to_string())].into(),
                    ..Provider::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        let args = run_with(
            &[
                "-p",
                "task",
                "--model",
                "freeinference/glm-5.3-flash",
                "--header",
                "x-team:b",
            ],
            settings,
        );
        assert_eq!(args.provider, Wire::OpenAiChat);
        assert_eq!(args.base_url.as_deref(), Some("https://fi.example/v1"));
        assert_eq!(args.api_key_env.as_deref(), Some("FI_KEY"));
        assert_eq!(args.model, "glm-5.3-flash");
        assert_eq!(
            args.header,
            vec![
                ("x-team".to_string(), "a".to_string()),
                ("x-team".to_string(), "b".to_string())
            ]
        );
        assert_eq!(args.policy, vec!["budget", "mask"]);
    }

    #[test]
    fn a_model_id_keeps_its_slashes() {
        let settings = Settings {
            providers: [("commandcode".to_string(), Provider::default())].into(),
            ..Settings::default()
        };
        let args = run_with(
            &[
                "-p",
                "task",
                "--model",
                "commandcode/deepseek/deepseek-v4-flash",
            ],
            settings,
        );
        assert_eq!(args.model, "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn a_name_without_a_provider_is_the_model_id() {
        let args = run(&["-p", "task", "--model", "claude-sonnet-5-5"]);
        assert_eq!(args.model, "claude-sonnet-5-5");
        assert_eq!(args.provider, Wire::Anthropic);
        let args = run(&["-p", "task", "--model", "meta/llama"]);
        assert_eq!(args.model, "meta/llama");
        let args = run(&["-p", "task", "--model", "openai/gpt-5"]);
        assert_eq!(args.model, "gpt-5");
        assert_eq!(args.provider, Wire::OpenAiResponses);
        assert_eq!(args.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn model_settings_beat_top_level_ones() {
        let settings = Settings {
            model: Some("fi/glm".into()),
            max_tokens: Some(32_000),
            policy: Some(vec!["stuck".into()]),
            providers: providers(Provider {
                models: [(
                    "glm".to_string(),
                    Model {
                        max_tokens: Some(16_000),
                        context_window: Some(128_000),
                        policy: Some(vec!["snapshot".into()]),
                    },
                )]
                .into(),
                ..Provider::default()
            }),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task"], settings.clone());
        assert_eq!(args.max_tokens, 16_000);
        assert_eq!(args.context_window, Some(128_000));
        assert_eq!(args.policy, vec!["snapshot"]);
        let args = run_with(&["-p", "task", "--max-tokens", "8000"], settings);
        assert_eq!(args.max_tokens, 8000);
    }

    #[test]
    fn flags_beat_provider_settings() {
        let settings = Settings {
            providers: providers(Provider {
                wire: Some("openai-chat".into()),
                base_url: Some("https://fi.example/v1".into()),
                api_key_env: Some("FI_KEY".into()),
                ..Provider::default()
            }),
            ..Settings::default()
        };
        let args = run_with(
            &[
                "-p",
                "task",
                "--model",
                "fi/glm",
                "--base-url",
                "http://localhost:1",
                "--api-key-env",
                "MINE",
                "--provider",
                "anthropic",
            ],
            settings,
        );
        assert_eq!(args.base_url.as_deref(), Some("http://localhost:1"));
        assert_eq!(args.api_key_env.as_deref(), Some("MINE"));
        assert_eq!(args.provider, Wire::Anthropic);
    }
}
