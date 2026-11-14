//! Command line argument parsing with `lexopt`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use reagent_ai::{Auth, Compat, CompatOverride, MaxTokensField, Preset, Route};
use reagent_config::mcp::{McpConfig, McpDirect};
use reagent_config::permissions::Rules;
use reagent_config::{Layer, Layers, McpServer, Model, Provider, Secret, Settings};
use serde::Deserialize;
use serde::de::IntoDeserializer;

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
/// a deadline or a turn cap, and with one it cut timeouts on a slow model. The
/// snapshot hands the model its environment in the first message, so it does
/// not spend a turn finding out.
const DEFAULT_POLICIES: [&str; 2] = ["budget", "snapshot"];

/// The policies with no `--policy` flag on the chat wire. Masking cut prompt
/// tokens by 42% on a glm-5.3-flash dev run with no drop in pass rate.
const DEFAULT_CHAT_POLICIES: [&str; 3] = ["budget", "mask", "snapshot"];

/// The reply token cap when `--max-tokens` is absent. A `write` call carries
/// the whole file, and thinking counts toward the cap, so a low cap cuts the
/// reply.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// The tool names a `--tools` value may select.
pub const TOOL_NAMES: [&str; 8] = [
    "bash", "edit", "find", "grep", "read", "write", "codemode", "ask",
];

/// Whether the folded `--tools` list writes a plain list at all, and whether
/// it names `codemode`.
fn codemode_in_tools(folded: &[String]) -> (bool, bool) {
    let plain = folded.iter().any(|entry| !reagent_config::is_edit(entry));
    let named = folded
        .iter()
        .any(|entry| reagent_config::list_name(entry) == reagent_script::codemode::NAME);
    (plain, named)
}

/// The tools with no `--tools` flag. `codemode` joins them only when asked
/// for: a script the model writes costs a turn more than the calls it saves.
pub(crate) const DEFAULT_TOOLS: [&str; 7] =
    ["bash", "edit", "find", "grep", "read", "write", "ask"];

/// What a run says when a tool list leaves `codemode` out and drops the servers
/// whose tools all need one, naming them.
fn mcp_without_codemode(dropped: &[&str]) -> String {
    format!(
        "reagent: MCP servers need codemode; they are off because the tool list leaves codemode out: {}",
        dropped.join(", ")
    )
}

/// The provider wire selected by `--provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    /// The Anthropic Messages API.
    Anthropic,
    /// OpenAI-compatible Chat Completions.
    OpenAiChat,
    /// The OpenAI Responses API.
    OpenAiResponses,
    /// The pi-messages API.
    PiMessages,
    /// A scripted provider read from `fake:<file>`, testing builds only.
    #[cfg(feature = "testing")]
    Fake(String),
}

impl Wire {
    /// The preset wire this is, if it is one.
    fn to_ai(&self) -> Option<reagent_ai::Wire> {
        match self {
            Wire::Anthropic => Some(reagent_ai::Wire::Anthropic),
            Wire::OpenAiChat => Some(reagent_ai::Wire::OpenAiChat),
            Wire::OpenAiResponses => Some(reagent_ai::Wire::OpenAiResponses),
            Wire::PiMessages => Some(reagent_ai::Wire::PiMessages),
            #[cfg(feature = "testing")]
            Wire::Fake(_) => None,
        }
    }

    /// The wire of a preset.
    fn from_ai(wire: reagent_ai::Wire) -> Self {
        match wire {
            reagent_ai::Wire::Anthropic => Wire::Anthropic,
            reagent_ai::Wire::OpenAiChat => Wire::OpenAiChat,
            reagent_ai::Wire::OpenAiResponses => Wire::OpenAiResponses,
            reagent_ai::Wire::PiMessages => Wire::PiMessages,
        }
    }

    /// The environment variable holding the API key when `--api-key-env` is
    /// absent.
    pub fn default_api_key_env(&self) -> &'static str {
        match self {
            Wire::Anthropic => reagent_ai::Wire::Anthropic.default_api_key_env(),
            Wire::OpenAiChat => reagent_ai::Wire::OpenAiChat.default_api_key_env(),
            Wire::OpenAiResponses => reagent_ai::Wire::OpenAiResponses.default_api_key_env(),
            Wire::PiMessages => reagent_ai::Wire::PiMessages.default_api_key_env(),
            #[cfg(feature = "testing")]
            Wire::Fake(_) => reagent_ai::Wire::Anthropic.default_api_key_env(),
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
            Wire::PiMessages => ("RADIUS_BASE_URL", "https://radius.pi.dev/v1"),
            #[cfg(feature = "testing")]
            Wire::Fake(_) => ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
        };
        std::env::var(variable)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    }
}

/// What a run does with its session log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Sessions {
    /// Start a new session; the default.
    #[default]
    New,
    /// Write no session log at all.
    None,
    /// Continue the newest session of the working folder.
    Continue,
    /// Resume the session a reference names, or list the sessions when the
    /// reference is absent.
    Resume(Option<String>),
    /// Start a new session forked from a reference after one of its turns.
    Fork {
        /// The session to fork: an id prefix or a session folder.
        reference: String,
        /// The turn to fork after, or the last one when absent.
        turn: Option<u64>,
    },
}

/// The parsed command line.
#[derive(Debug)]
pub struct Args {
    /// The task the agent should complete, or where to read it from.
    pub task: Option<TaskSource>,
    /// What the run does with the session store.
    pub sessions: Sessions,
    /// Whether `--model` named the model.
    pub model_given: bool,
    /// Whether `--provider` named the provider wire.
    pub provider_given: bool,
    /// Print one JSON object per event.
    pub json: bool,
    /// Keep the terminal UI inline, under the shell's scrollback.
    #[cfg_attr(
        all(not(feature = "tui"), not(test)),
        expect(dead_code, reason = "only the TUI and the tests read it")
    )]
    pub inline: bool,
    /// Serve the session as a worker instead of running it once.
    pub worker: bool,
    /// Read protocol requests from standard input while the run goes on.
    pub commands: bool,
    /// Take clients on the run's socket.
    pub listen: bool,
    /// Serve the run's socket by default, so a client can steer, queue or cancel
    /// it; `--no-serve` turns that off.
    pub serve: bool,
    /// The provider model id.
    pub model: String,
    /// The provider wire to use.
    pub provider: Wire,
    /// The endpoint URL, or the provider default when absent.
    pub base_url: Option<String>,
    /// The compat flags of a preset or a `compat` table, or the plain wire
    /// when absent.
    pub compat: Option<Compat>,
    /// The name of the environment variable holding the API key, or the
    /// provider default when absent.
    pub api_key_env: Option<String>,
    /// The API key from the provider table of the config, if any.
    pub api_key: Option<Secret>,
    /// The config or built-in provider name from `--model name/id`, if any.
    pub provider_name: Option<String>,
    /// The preset the provider takes its defaults from, if any.
    pub preset: Option<&'static Preset>,
    /// The header the key goes in.
    pub auth: Auth,
    /// The saved login this provider signs in with, if it has a name.
    pub login: Option<String>,
    /// Extra request headers from `--header`, in order.
    pub header: Vec<(String, String)>,
    /// The cap on reply tokens, thinking included.
    pub max_tokens: u32,
    /// How much reasoning the provider may spend, in its own words: a level
    /// text or a token budget, as written.
    pub reasoning: Option<String>,
    /// A hard deadline in seconds, if any.
    pub deadline: Option<u64>,
    /// The `[codemode]` table, merged from the config files.
    pub codemode: reagent_config::CodemodeSettings,
    /// The `[session]` table, merged from the config files.
    pub session: reagent_config::SessionSettings,
    /// The tools the run offers, resolved and sorted.
    pub tools: Vec<String>,
    /// The MCP servers the run starts, picked and sorted by name.
    pub mcp_servers: Vec<(String, McpServer)>,
    /// Whether a run warns when many MCP tools go into the request directly.
    pub warn_many_direct: bool,
    /// The working directory the tools run in.
    pub cwd: PathBuf,
    /// The loop policies to enable, resolved and sorted.
    pub policy: Vec<String>,
    /// Folder names from the config that searches leave out.
    pub skip: Vec<String>,
    /// Whether a plain file read in bash runs as a `read` call.
    pub bash_reads: bool,
    /// The profile file the run read, if any.
    pub profile: Option<String>,
    /// The permission rules of the config files.
    pub rules: Rules,
    /// The context window of the model in tokens, from its config table.
    pub context_window: Option<u64>,
    /// What the model costs, from its config table.
    pub price: Option<reagent_config::ModelPrice>,
    /// How bash commands are fenced in.
    pub sandbox: SandboxMode,
    /// Whether sandboxed commands may use the network.
    pub network: bool,
    /// Folders sandboxed commands may write, besides the working and temp
    /// folders.
    pub write: Vec<PathBuf>,
    /// The folder the run loads its plugins from, `None` when `--no-plugins`
    /// is given or the folder does not exist.
    pub plugins_dir: Option<PathBuf>,
}

/// One provider the `models` command lists.
#[derive(Debug)]
pub struct Target {
    /// The provider name, as in `--model name/id`.
    pub name: String,
    /// The saved login this provider signs in with: its `login`, else its name.
    pub login: String,
    /// The preset the provider takes its defaults from, if any.
    pub preset: Option<&'static Preset>,
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
    /// The model list path under the base URL.
    pub models_path: &'static str,
    /// The models.dev provider id its list is filled from, when the config
    /// names one.
    pub models_dev: Option<String>,
}

/// The `models` command.
#[derive(Debug)]
pub struct ModelsArgs {
    /// The providers to list.
    pub targets: Vec<Target>,
    /// Fetch even when the cache is fresh.
    pub refresh: bool,
    /// The profile file the command read, if any.
    pub profile: Option<String>,
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
        "      --inline         keep the terminal UI inline, under the shell's scrollback\n",
        "      --commands       read protocol requests (send, cancel, approve, set, compact) from stdin,\n",
        "                       one JSON-RPC line each; needs --json\n",
        "      --listen         take clients on the run's socket, found in the discovery folder by session id\n",
        "      --no-serve       take no clients on the run's socket; a -p run serves it by default\n",
        "      --model <id>     the provider model id\n",
        "      --provider <wire>  anthropic, openai-chat, openai-responses or pi-messages\n",
        "                       (default: anthropic)\n",
        "                       --model <name>/<id> picks a preset: anthropic, openai, chatgpt,\n",
        "                       openrouter, opencode-go, commandcode, deepseek, freeinference,\n",
        "                       radius or synthetic\n",
        "      --base-url <url>   the endpoint; default from the provider env, else built in\n",
        "      --api-key-env <name>  the env var holding the API key; never the key itself\n",
        "      --header <name:value>  an extra request header; repeatable; reserved names\n",
        "                       (authorization, x-api-key, content-type, anthropic-version) are errors\n",
        "      --max-tokens <n> the reply token cap, thinking included (default: 64000)\n",
        "      --reasoning <text>  the provider's reasoning level, or a token budget; off = none\n",
        "      --deadline <s>   stop after this many seconds\n",
        "      --cwd <dir>      the working directory for the tools (default: current dir)\n",
        "      --profile <name>  read <config dir>/profiles/<name>.toml over the global config;\n",
        "                       REAGENT_PROFILE names one too\n",
        "  -t, --tools <list>   tools: bash, edit, find, grep, read, write, codemode, ask;\n",
        "                       +name adds, -name removes, plain names replace (default: all but codemode)\n",
        "      --mcp <list>     MCP servers to start: +name adds, -name removes, plain names replace\n",
        "      --no-mcp         start no MCP server\n",
        "      --no-plugins     load no plugins from the config folder\n",
        "      --policy <list>  loop policies: completion, stuck, budget, snapshot, mask, compact;\n",
        "                       all = completion, budget, snapshot; none = off;\n",
        "                       +name adds, -name removes, plain names replace\n",
        "                       (default: budget and snapshot, plus mask on openai-chat)\n",
        "      --version        print the version and exit\n",
        "\n",
        "Commands:\n",
        "  models [name] [--refresh]  list the models of a provider, or of all with a key\n",
        "  login <provider> [--as <name>] [--no-browser]  save a login for a provider;\n",
        "                       openrouter, chatgpt\n",
        "  logout <provider>  drop a saved login\n",
        "  mcp add|remove|list|login|logout  manage MCP servers (see reagent mcp --help)\n",
        "  plugin add|list|remove|update  install a plugin from npm (see reagent plugin --help)\n",
        "  rpc <session>  print the requests a live session waits for, then send the request\n",
        "                       lines of stdin, one request each, and print their replies\n"
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

/// Maps a `--provider` value to a wire, or to `fake:<file>` under testing.
fn parse_provider(value: &str) -> Result<Wire, String> {
    match reagent_ai::Wire::parse(value) {
        Some(wire) => Ok(Wire::from_ai(wire)),
        None => {
            let other = value;
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

/// Splits a `--tools` or `--policy` comma list into its trimmed entries.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// The edits a plain `none` layer becomes: every policy name removed.
fn clear_policies() -> Vec<String> {
    POLICY_NAMES.iter().map(|name| format!("-{name}")).collect()
}

/// Checks a `--tools` comma list, which exits 2 on an unknown name.
fn tool_list(value: &str) -> Result<Vec<String>, String> {
    let names = split_list(value);
    reagent_config::check_list(&names)?;
    for entry in &names {
        let name = reagent_config::list_name(entry);
        if !TOOL_NAMES.contains(&name) {
            return Err(format!(
                "unknown tool: {name}; tools: {}",
                TOOL_NAMES.join(", ")
            ));
        }
    }
    Ok(names)
}

/// Splits a `--mcp` comma list; an unknown name fails once the config is read.
fn mcp_list(value: &str) -> Result<Vec<String>, String> {
    let names = split_list(value);
    reagent_config::check_list(&names)?;
    Ok(names)
}

/// The servers `names` picks, sorted by name, or the error that names an
/// unknown one beside the servers the config defines.
fn pick_servers(
    names: &[String],
    servers: &BTreeMap<String, McpServer>,
) -> Result<Vec<(String, McpServer)>, String> {
    let mut picked = Vec::with_capacity(names.len());
    for name in names {
        let Some(server) = servers.get(name) else {
            let defined: Vec<&str> = servers.keys().map(String::as_str).collect();
            return Err(if defined.is_empty() {
                format!("unknown server: {name}; no server is defined in mcp.toml")
            } else {
                format!("unknown server: {name}; servers: {}", defined.join(", "))
            });
        };
        picked.push((name.clone(), server.clone()));
    }
    Ok(picked)
}

/// Checks a policy list, expanding `all` and spelling a plain `none` as the
/// edits that clear the list. An unknown name is an error.
fn expand_policy(names: &[String]) -> Result<Vec<String>, String> {
    reagent_config::check_list(names)?;
    if names.iter().any(|entry| reagent_config::is_edit(entry)) {
        for entry in names {
            let name = reagent_config::list_name(entry);
            if !POLICY_NAMES.contains(&name) {
                return Err(format!("unknown policy: {name}"));
            }
        }
        return Ok(names.to_vec());
    }
    if names.len() > 1 && names.iter().any(|name| name == "none") {
        return Err("none must be the only policy".to_string());
    }
    let mut expanded = Vec::with_capacity(names.len());
    for name in names {
        match name.as_str() {
            "none" => return Ok(clear_policies()),
            "all" => expanded.extend(ALL_POLICIES.iter().map(ToString::to_string)),
            name if POLICY_NAMES.contains(&name) => expanded.push(name.to_string()),
            other => return Err(format!("unknown policy: {other}")),
        }
    }
    expanded.sort();
    expanded.dedup();
    Ok(expanded)
}

/// Parses the process arguments, or returns an exit decision.
///
/// `worker` serves the session instead of running it once, and `ui` lets a run
/// with no task start the terminal UI: a client then sends the messages.
pub fn parse(argv: Vec<std::ffi::OsString>, worker: bool, ui: bool) -> Result<Outcome, String> {
    parse_from(lexopt::Parser::from_args(argv), &load_settings, worker, ui)
}

/// Takes the leading `worker` word, which makes the run a session worker.
pub fn worker_word(argv: &mut Vec<std::ffi::OsString>) -> bool {
    if argv.first().and_then(|word| word.to_str()) != Some("worker") {
        return false;
    }
    argv.remove(0);
    true
}

/// Parses the process arguments again with `model` named.
///
/// A run that continues a session takes the provider and the model that
/// session ran with, and naming them here resolves them exactly as a `--model`
/// flag does: the provider table or preset, its endpoint, key variable, headers
/// and auth. A caller names them only when the arguments name no model,
/// provider or profile of their own. It reads the config files, so call it
/// inside `spawn_blocking`.
pub fn reparse(model: &str) -> Result<Outcome, String> {
    let mut argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let worker = worker_word(&mut argv);
    let argv = argv.into_iter().chain(["--model".into(), model.into()]);
    parse_from(
        lexopt::Parser::from_args(argv),
        &load_settings,
        worker,
        false,
    )
}

/// Checks the policy and tool lists of `layer`, expanding `all` and `none`,
/// and names the file when one is wrong.
fn check_lists(layer: &mut Layer) -> Result<(), String> {
    let path = layer.path.display().to_string();
    let expand = |names: &mut Option<Vec<String>>| -> Result<(), String> {
        if let Some(list) = names {
            *list = expand_policy(list).map_err(|error| format!("{path}: {error}"))?;
        }
        Ok(())
    };
    expand(&mut layer.settings.policy)?;
    for provider in layer.settings.providers.values_mut() {
        for model in provider.models.values_mut() {
            expand(&mut model.policy)?;
        }
    }
    if let Some(list) = &layer.settings.tools {
        tool_list(&list.join(",")).map_err(|error| format!("{path}: {error}"))?;
    }
    if let Some(list) = &layer.settings.mcp {
        reagent_config::check_list(list).map_err(|error| format!("{path}: {error}"))?;
    }
    reagent_config::check_list(&layer.settings.skip).map_err(|error| format!("{path}: {error}"))?;
    Ok(())
}

/// The `[session]` limits the config files of `cwd` set, read the way a run
/// reads them: the global file first, then the profile the environment names.
///
/// Reading the files blocks, so call it in `spawn_blocking`.
pub fn session_limits(cwd: &Path) -> Result<reagent_config::SessionSettings, String> {
    let profile = profile_from(None, std::env::var("REAGENT_PROFILE").ok());
    let (settings, _, _) = load_settings(cwd, profile.as_deref())?;
    Ok(settings.session)
}

/// The `ui` setting the config files of `cwd` give for the profile `profile`,
/// read and merged the way a run reads them.
///
/// An unknown value is an error that names the key. Nothing here starts a
/// worker or writes anything, so a typo stops the start on its own. Reading the
/// files blocks, so call it in `spawn_blocking`.
#[cfg(feature = "tui")]
pub fn ui_setting(cwd: &Path, profile: Option<&str>) -> Result<Option<reagent_config::Ui>, String> {
    let layers = reagent_config::load_for(cwd, profile)?;
    reagent_config::merge_all(&layers).ui()
}

/// Loads the config files for `cwd` and the profile `profile`, checks their
/// lists, merges them, parses their permission rules and reads their servers.
fn load_settings(cwd: &Path, profile: Option<&str>) -> Result<Loaded, String> {
    let layers = reagent_config::load_for(cwd, profile)?;
    let servers = reagent_config::load_mcp_for(profile)?;
    let (settings, rules) = settings_from(layers, cwd, &servers.servers)?;
    Ok((settings, rules, servers))
}

/// The plugins folder under the global config folder `config`, when the folder
/// exists.
///
/// The callers read it where the config files are read, before the runtime
/// starts or in `spawn_blocking`, so it costs a run that has no plugins folder
/// one stat, no job and no thread.
#[expect(
    clippy::disallowed_methods,
    reason = "the config is read before the runtime starts or in spawn_blocking"
)]
fn plugins_dir(config: Option<&Path>) -> Option<PathBuf> {
    let dir = config?.join("plugins");
    std::fs::metadata(&dir).ok()?.is_dir().then_some(dir)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the config is read before the runtime starts or in spawn_blocking"
)]
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn settings_from(
    mut layers: Layers,
    cwd: &Path,
    servers: &BTreeMap<String, McpServer>,
) -> Result<(Settings, Rules), String> {
    for layer in [&mut layers.global, &mut layers.profile, &mut layers.repo]
        .into_iter()
        .flatten()
    {
        check_lists(layer)?;
    }
    if let Some(repo) = &mut layers.repo {
        repo_mcp(repo, servers);
    }
    // The tools resolve paths from the real folder, so the rules must too,
    // or a symlinked --cwd would make every relative path rule miss.
    let cwd = canonical(cwd);
    let home = std::env::var_os("HOME").map(|home| canonical(Path::new(&home)));
    let rules = Rules::from_layers(&layers.ordered(), &cwd, home.as_deref())?;
    // A saved rule is an approval a client gave in an earlier session, so it
    // answers the ask rule that asked for it, as an approval of this session
    // does; as a plain allow rule every ask rule would win over it. A deny
    // rule still wins over both.
    if let Some(saved) = &layers.saved {
        let file = saved.path.display().to_string();
        for rule in &saved.settings.permissions.allow {
            rules.add_saved_allow(rule, &file);
        }
    }
    Ok((reagent_config::merge_all(&layers), rules))
}

/// Cuts `repo`'s `mcp` list down to what a repo file may do: remove a server,
/// or name one that is enabled. A server is something the user trusts by
/// defining it in `mcp.toml`, and a cloned repo is not trusted.
fn repo_mcp(repo: &mut Layer, servers: &BTreeMap<String, McpServer>) {
    let Some(list) = &mut repo.settings.mcp else {
        return;
    };
    let path = repo.path.clone();
    list.retain(|entry| {
        let name = reagent_config::list_name(entry);
        let starts = !entry.starts_with('-') && servers.get(name).is_some_and(|s| !s.enabled);
        if starts {
            eprintln!("{}", repo_disabled(&path, name));
        }
        !starts
    });
}

/// The line a run prints once for a repo file that asks for a disabled server.
fn repo_disabled(path: &Path, name: &str) -> String {
    format!(
        "reagent: {} asks for MCP server {name}, which is disabled; enable it in mcp.toml or pass --mcp +{name}",
        path.display()
    )
}

/// The preset a name selects, whose fields a config table may override.
fn built_in_provider(name: &str) -> Option<&'static Preset> {
    reagent_ai::providers::get(name)
}

/// The preset a table takes its defaults from: its `preset`, else the preset
/// named like the table.
fn preset_for(
    name: &str,
    table: Option<&reagent_config::Provider>,
) -> Result<Option<&'static Preset>, String> {
    let Some(preset_name) = table.and_then(|table| table.preset.as_deref()) else {
        return Ok(built_in_provider(name));
    };
    reagent_ai::providers::get(preset_name)
        .map(Some)
        .ok_or_else(|| {
            let names: Vec<&str> = reagent_ai::providers::ALL
                .iter()
                .map(|preset| preset.name)
                .collect();
            format!(
                "config: [providers.{name}]: unknown preset {preset_name}; presets: {}",
                names.join(", ")
            )
        })
}

/// The rule a provider table name and a `login` value share: letters, digits,
/// `-` and `_` only, which a login file name and a cache file name both take.
pub fn check_account_name(name: &str) -> Result<(), String> {
    if reagent_config::valid_name(name) {
        Ok(())
    } else {
        Err(format!(
            "provider name {name}: use letters, digits, - and _ only, not starting with -"
        ))
    }
}

/// The profile a run reads: the flag, else `REAGENT_PROFILE` when it is set
/// and not empty.
pub(crate) fn profile_from(flag: Option<String>, env: Option<String>) -> Option<String> {
    flag.or_else(|| env.filter(|name| !name.is_empty()))
}

/// The profile name rule, which the provider name rule already gives: a name
/// is a file name too.
fn check_profile_name(name: &str) -> Result<(), String> {
    check_account_name(name).map_err(|_| {
        format!("invalid profile {name}: use letters, digits, - and _ only, not starting with -")
    })
}

/// Checks every provider table name and `login` value, and that every
/// `preset` names a known preset, so a typo fails before any run.
fn check_provider_names(settings: &Settings) -> Result<(), String> {
    for (name, table) in &settings.providers {
        check_account_name(name).map_err(|error| format!("config: {error}"))?;
        if let Some(login) = &table.login {
            check_account_name(login).map_err(|error| format!("config: {error}"))?;
        }
        preset_for(name, Some(table))?;
    }
    Ok(())
}

/// The compat flags of a run: the config flags over the route's, or over the
/// host rule of the endpoint when there is no route. `None` when there is no
/// preset and no config flag.
fn resolve_compat(
    name: Option<&str>,
    table: Option<&reagent_config::Table>,
    preset: Option<&Preset>,
    route: Option<&Route>,
    base_url: &str,
) -> Result<Option<Compat>, String> {
    let over = match table {
        Some(table) => {
            CompatOverride::deserialize(table.clone().into_deserializer()).map_err(|error| {
                format!(
                    "config: [providers.{}.compat]: {error}",
                    name.unwrap_or_default()
                )
            })?
        }
        None => CompatOverride::default(),
    };
    // A route's flags describe its own endpoint; another URL gets the host rule.
    let base = match (route, table) {
        (Some(route), _) if base_url.trim_end_matches('/') == route.base_url => route.compat,
        (None, None) if preset.is_none() => return Ok(None),
        _ => {
            let field = if base_url.contains("api.openai.com") {
                MaxTokensField::MaxCompletionTokens
            } else {
                MaxTokensField::MaxTokens
            };
            CompatOverride {
                max_tokens_field: Some(field),
                reasoning_field: None,
                reasoning_off: None,
            }
            .apply(Compat::DEFAULT)
        }
    };
    Ok(Some(over.apply(base)))
}

/// Splits the model text at its first `/` when the part before it names a
/// preset or configured provider.
fn split_model<'a>(text: &'a str, settings: &Settings) -> Option<(&'a str, &'a str)> {
    let (name, id) = text.split_once('/')?;
    (settings.providers.contains_key(name) || built_in_provider(name).is_some())
        .then_some((name, id))
}

/// The providers `models` lists: the one `name` selects, else every config
/// table and every built-in provider.
fn model_targets(settings: &Settings, name: Option<&str>) -> Result<Vec<Target>, String> {
    let mut names: Vec<&str> = settings.providers.keys().map(String::as_str).collect();
    for preset in reagent_ai::providers::ALL {
        if !names.contains(&preset.name) {
            names.push(preset.name);
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
            let built_in = preset_for(name, settings.providers.get(name))?;
            let login = table.login.clone().unwrap_or_else(|| name.to_string());
            // A preset's list lives on its default route whatever wire a run
            // names; a table with its own endpoint lists from there.
            let route = built_in.map(Preset::default_route);
            let named = table
                .wire
                .as_deref()
                .map(parse_provider)
                .transpose()
                .map_err(|error| format!("config: {error}"))?;
            let wire = match (&table.base_url, named, route) {
                (None, _, Some(route)) | (Some(_), None, Some(route)) => Wire::from_ai(route.wire),
                (Some(_), Some(wire), _) | (None, Some(wire), None) => wire,
                (_, None, None) => Wire::Anthropic,
            };
            let mut header = Vec::new();
            for (key, value) in &table.headers {
                reagent_ai::check_header(key, value).map_err(|error| format!("config: {error}"))?;
                header.push((key.clone(), value.clone()));
            }
            Ok(Target {
                name: name.to_string(),
                login,
                preset: built_in,
                wire,
                base_url: table.base_url.or_else(|| route.map(Route::endpoint)),
                api_key_env: table
                    .api_key_env
                    .or_else(|| built_in.map(|preset| preset.api_key_env.to_string())),
                api_key: table.api_key,
                header,
                implicit: name_given.is_none() && !settings.providers.contains_key(name),
                models_path: built_in.map_or("/models", |preset| preset.models_path),
                models_dev: table.models_dev,
            })
        })
        .collect()
}

/// The merged settings, the permission rules and the MCP servers a run reads.
type Loaded = (Settings, Rules, McpConfig);

/// Reads the merged settings, the permission rules and the MCP servers for a
/// working folder and a profile.
type LoadSettings<'a> = dyn Fn(&Path, Option<&str>) -> Result<Loaded, String> + 'a;

/// Sets the session choice, refusing a second one: the four flags exclude
/// each other.
fn choose(sessions: &mut Sessions, picked: Sessions) -> Result<(), String> {
    if !matches!(sessions, Sessions::New) {
        return Err("use only one of -c, -r, --fork and --no-session".into());
    }
    *sessions = picked;
    Ok(())
}

/// Sets the reference of a `-r` whose value is not attached to the flag.
///
/// `-r` alone asks for the list, and the argument after it names the session,
/// so the two are one choice rather than a second one.
fn resume_ref(sessions: &mut Sessions, reference: String) -> Result<(), String> {
    if matches!(sessions, Sessions::Resume(None)) {
        *sessions = Sessions::Resume(Some(reference));
        return Ok(());
    }
    choose(sessions, Sessions::Resume(Some(reference)))
}

/// Splits a `--fork` value into its reference and the turn to fork after.
fn fork_ref(text: &str) -> Result<(String, Option<u64>), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(bad("--fork"));
    }
    match text.rsplit_once('@') {
        Some((reference, turn))
            if !reference.is_empty()
                && !turn.is_empty()
                && turn.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            Ok((
                reference.to_string(),
                Some(turn.parse().map_err(|_| bad("--fork"))?),
            ))
        }
        _ => Ok((text.to_string(), None)),
    }
}

/// Parses `args` (the arguments after the program name) into an outcome.
/// `load` reads the settings for the working directory; a flag beats them.
fn parse_from(
    mut parser: lexopt::Parser,
    load: &LoadSettings,
    worker: bool,
    ui: bool,
) -> Result<Outcome, String> {
    let mut task: Option<TaskSource> = None;
    let mut sessions = Sessions::New;
    let mut model_given = false;
    let mut provider_given = false;
    // A `--resume` whose reference is the next argument.
    let mut resume_value = false;
    let mut json = false;
    let mut inline = false;
    let mut commands = false;
    let mut listen = false;
    let mut no_serve = false;
    let mut model = None;
    let mut provider = None;
    let mut base_url = None;
    let mut api_key_env = None;
    let mut header = Vec::new();
    let mut max_tokens = None;
    let mut reasoning = None;
    let mut deadline = None;
    let mut cwd = None;
    // Each `--tools`, `--policy` and `--mcp` value, in command-line order.
    let mut tools: Vec<Vec<String>> = Vec::new();
    let mut policy: Vec<Vec<String>> = Vec::new();
    let mut mcp: Vec<Vec<String>> = Vec::new();
    let mut no_mcp = false;
    let mut no_plugins = false;
    let mut models: Option<Option<String>> = None;
    let mut refresh = false;
    let mut profile = None;
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
            lexopt::Arg::Long("inline") => inline = true,
            lexopt::Arg::Long("commands") => commands = true,
            lexopt::Arg::Long("listen") => listen = true,
            lexopt::Arg::Long("no-serve") => no_serve = true,
            lexopt::Arg::Long("model") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --model".to_string())?;
                model = Some(value.into_string().map_err(|_| bad("--model"))?);
                model_given = true;
            }
            lexopt::Arg::Short('c') | lexopt::Arg::Long("continue") => {
                choose(&mut sessions, Sessions::Continue)?;
            }
            lexopt::Arg::Short('r') | lexopt::Arg::Long("resume") => {
                match parser.optional_value() {
                    Some(value) => {
                        let reference = value.into_string().map_err(|_| bad("--resume"))?;
                        resume_ref(&mut sessions, reference)?;
                    }
                    None => {
                        resume_value = true;
                        choose(&mut sessions, Sessions::Resume(None))?;
                    }
                }
            }
            lexopt::Arg::Long("fork") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --fork".to_string())?;
                let text = value.into_string().map_err(|_| bad("--fork"))?;
                let (reference, turn) = fork_ref(&text)?;
                choose(&mut sessions, Sessions::Fork { reference, turn })?;
            }
            lexopt::Arg::Long("no-session") => choose(&mut sessions, Sessions::None)?,
            lexopt::Arg::Long("provider") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --provider".to_string())?;
                let text = value.into_string().map_err(|_| bad("--provider"))?;
                provider = Some(parse_provider(&text)?);
                provider_given = true;
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
            lexopt::Arg::Long("reasoning") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --reasoning".to_string())?;
                let text = value.into_string().map_err(|_| bad("--reasoning"))?;
                let text = text.trim();
                if text.is_empty() {
                    return Err(bad("--reasoning"));
                }
                reasoning = Some(text.to_string());
            }
            lexopt::Arg::Long("deadline") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --deadline".to_string())?;
                let text = value.into_string().map_err(|_| bad("--deadline"))?;
                deadline = Some(text.parse().map_err(|_| bad("--deadline"))?);
            }
            lexopt::Arg::Short('t') | lexopt::Arg::Long("tools") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --tools".to_string())?;
                let text = value.into_string().map_err(|_| bad("--tools"))?;
                tools.push(tool_list(&text).map_err(|error| format!("--tools: {error}"))?);
            }
            lexopt::Arg::Long("policy") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --policy".to_string())?;
                let text = value.into_string().map_err(|_| bad("--policy"))?;
                let names = split_list(&text);
                policy.push(expand_policy(&names).map_err(|error| format!("--policy: {error}"))?);
            }
            lexopt::Arg::Long("mcp") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --mcp".to_string())?;
                let text = value.into_string().map_err(|_| bad("--mcp"))?;
                mcp.push(mcp_list(&text).map_err(|error| format!("--mcp: {error}"))?);
            }
            lexopt::Arg::Long("no-mcp") => no_mcp = true,
            lexopt::Arg::Long("no-plugins") => no_plugins = true,
            lexopt::Arg::Long("cwd") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --cwd".to_string())?;
                cwd = Some(PathBuf::from(value));
            }
            lexopt::Arg::Long("profile") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --profile".to_string())?;
                profile = Some(value.into_string().map_err(|_| bad("--profile"))?);
            }
            lexopt::Arg::Long("refresh") => refresh = true,
            lexopt::Arg::Value(value) if resume_value => {
                resume_value = false;
                let reference = value.into_string().map_err(|_| bad("--resume"))?;
                resume_ref(&mut sessions, reference)?;
            }
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

    let profile = profile_from(profile, std::env::var("REAGENT_PROFILE").ok());
    if let Some(name) = &profile {
        check_profile_name(name)?;
    }

    // The two flags that serve the requests of a run, checked before the ones
    // of a worker so an error names the flag that was given.
    if worker && commands {
        return Err(
            "--commands is not for a worker: it reads the requests its clients send".into(),
        );
    }
    if worker && listen {
        return Err("--listen is not for a worker: it takes clients already".into());
    }
    // A served run is a plain one, so it cannot also be a run that a person
    // answers: those flags say how the run takes the answers it has none to ask
    // for.
    if no_serve && listen {
        return Err("--no-serve cannot go with --listen: it takes clients".into());
    }
    if no_serve && commands {
        return Err("--no-serve cannot go with --commands: it takes requests on stdin".into());
    }
    if no_serve && worker {
        return Err("--no-serve cannot go with --worker: it takes clients already".into());
    }
    // A worker speaks the client protocol on stdout and needs a journal to
    // catch a client up from, so neither flag means anything for one.
    if worker && json {
        return Err("--json is not for a worker: it writes the client protocol".into());
    }
    if worker && matches!(sessions, Sessions::None) {
        return Err("--no-session is not for a worker: it needs a journal".into());
    }
    // A client of the protocol is told what happens to the session, and the
    // requests it answers name its records, so its run speaks `--json` and
    // keeps a journal.
    if commands && !json {
        return Err("--commands needs --json: the answers are event lines too".into());
    }
    if commands && matches!(task, Some(TaskSource::Stdin)) {
        return Err("--commands cannot read the task from stdin: -p takes the task text".into());
    }
    if matches!(sessions, Sessions::None) {
        if commands {
            return Err("--no-session is not for --commands: it needs a journal".into());
        }
        if listen {
            return Err("--no-session is not for --listen: it needs a journal".into());
        }
    }

    if let Some(name) = models {
        if task.is_some()
            || json
            || commands
            || listen
            || model.is_some()
            || provider.is_some()
            || base_url.is_some()
            || api_key_env.is_some()
            || !header.is_empty()
            || max_tokens.is_some()
            || reasoning.is_some()
            || deadline.is_some()
            || !tools.is_empty()
            || !policy.is_empty()
            || !mcp.is_empty()
            || no_mcp
            || no_plugins
        {
            return Err("the models command takes no run flags".into());
        }
        let (settings, _, _) = load(
            &cwd.unwrap_or_else(|| PathBuf::from(".")),
            profile.as_deref(),
        )?;
        check_provider_names(&settings)?;
        let targets = model_targets(&settings, name.as_deref())?;
        return Ok(Outcome::Models(Box::new(ModelsArgs {
            targets,
            refresh,
            profile,
        })));
    }
    if refresh {
        return Err("--refresh needs the models command".into());
    }
    // Only a continue, a resume, a worker, a run that takes its requests on
    // stdin or the terminal UI may run with no message: the run answers the
    // calls its ledger left open, a worker takes the ones its clients send, a
    // run with `--commands` takes them on stdin, and the UI sends its own.
    let open = matches!(sessions, Sessions::Continue | Sessions::Resume(_))
        || worker
        || commands
        || (ui && matches!(sessions, Sessions::New));
    if task.is_none() && !open {
        if json {
            return Err("the --json flag needs a task; use -p <task>".into());
        }
        return Err("missing task; use -p <task>".into());
    }
    let cwd = cwd.unwrap_or_else(|| PathBuf::from("."));
    // A `-p` run with a session serves its socket by default, so a client or
    // another agent can steer, queue or cancel it. `--no-serve` turns that off.
    let serve = !no_serve
        && !listen
        && !commands
        && !worker
        && task.is_some()
        && !matches!(sessions, Sessions::None);
    let (settings, rules, servers) = load(&cwd, profile.as_deref())?;
    // The plugins folder is read here, where the config files are, and not on
    // the blocking pool: a run with no plugins folder costs one stat before the
    // runtime starts, no job and no thread.
    let plugins_dir = if no_plugins {
        None
    } else {
        plugins_dir(reagent_config::global_dir().as_deref())
    };
    check_provider_names(&settings)?;
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
    let built_in = match provider_name {
        Some(name) => preset_for(name, table)?,
        None => None,
    };
    let table = table.cloned().unwrap_or_else(Provider::default);
    let model_table = table
        .models
        .get(&model_id)
        .cloned()
        .unwrap_or_else(Model::default);
    let named = match (provider, &table.wire) {
        (Some(wire), _) => Some(wire),
        (None, Some(name)) => {
            Some(parse_provider(name).map_err(|error| format!("config: {error}"))?)
        }
        (None, None) => None,
    };
    let base_url = base_url.or(table.base_url);
    let route = match built_in {
        Some(preset) => {
            let wire = named.as_ref().and_then(Wire::to_ai);
            let route = preset.route(&model_id, wire);
            if let (None, Some(wire), None) = (route, wire, &base_url) {
                return Err(format!(
                    "{}: no {} route for model {model_id}; routes: {}; or set base_url",
                    preset.name,
                    wire.name(),
                    preset.route_names(&model_id)
                ));
            }
            route
        }
        None => None,
    };
    let provider = match (named, route) {
        (Some(wire), _) => wire,
        (None, Some(route)) => Wire::from_ai(route.wire),
        (None, None) => Wire::Anthropic,
    };
    let mut config_header = Vec::new();
    for (name, value) in &table.headers {
        reagent_ai::check_header(name, value).map_err(|error| format!("config: {error}"))?;
        config_header.push((name.clone(), value.clone()));
    }
    config_header.extend(header);
    let header = config_header;
    // The lists fold in layer order: the config files, each flag as written,
    // and for `policy` the model's table before the flags.
    let mut folded = settings.policy.clone().unwrap_or_default();
    if let Some(list) = &model_table.policy {
        reagent_config::fold_list(&mut folded, list, true)
            .map_err(|error| format!("config: {error}"))?;
    }
    for list in &policy {
        reagent_config::fold_list(&mut folded, list, true)
            .map_err(|error| format!("--policy: {error}"))?;
    }
    let default: &[&str] = if provider == Wire::OpenAiChat {
        &DEFAULT_CHAT_POLICIES
    } else {
        &DEFAULT_POLICIES
    };
    let policy = reagent_config::resolve_list(&folded, default);
    let mut folded = settings.tools.clone().unwrap_or_default();
    for list in &tools {
        reagent_config::fold_list(&mut folded, list, true)
            .map_err(|error| format!("--tools: {error}"))?;
    }
    let mut tools = reagent_config::resolve_list(&folded, &DEFAULT_TOOLS);
    let (plain_tools, names_codemode) = codemode_in_tools(&folded);
    // The servers fold the same way, with every enabled server as the base.
    let mut folded = settings.mcp.clone().unwrap_or_default();
    for list in &mcp {
        reagent_config::fold_list(&mut folded, list, true)
            .map_err(|error| format!("--mcp: {error}"))?;
    }
    let default: Vec<&str> = servers
        .servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .map(|(name, _)| name.as_str())
        .collect();
    let mut mcp_servers = if no_mcp {
        Vec::new()
    } else {
        pick_servers(
            &reagent_config::resolve_list(&folded, &default),
            &servers.servers,
        )?
    };
    // A script reaches the tools that are not direct, so a picked server with
    // one needs `codemode`. A server whose every tool is direct needs nothing.
    if mcp_servers
        .iter()
        .any(|(_, server)| server.direct != McpDirect::All)
    {
        if !plain_tools && !names_codemode {
            tools.push(reagent_script::codemode::NAME.to_string());
            tools.sort();
        } else if !tools
            .iter()
            .any(|name| name == reagent_script::codemode::NAME)
        {
            let dropped: Vec<&str> = mcp_servers
                .iter()
                .filter(|(_, server)| server.direct == McpDirect::None)
                .map(|(name, _)| name.as_str())
                .collect();
            if !dropped.is_empty() {
                eprintln!("{}", mcp_without_codemode(&dropped));
            }
            mcp_servers.retain(|(_, server)| server.direct != McpDirect::None);
        }
    }
    // Editing old tool results breaks the reasoning the Anthropic and
    // Responses wires replay, so masking runs only on the chat wire.
    if policy.iter().any(|name| name == "mask") && provider != Wire::OpenAiChat {
        return Err("--policy mask needs --provider openai-chat".into());
    }

    let base_url = base_url.or_else(|| route.map(Route::endpoint));
    let compat = resolve_compat(
        provider_name,
        table.compat.as_ref(),
        built_in,
        route,
        base_url
            .clone()
            .unwrap_or_else(|| provider.base_url())
            .as_str(),
    )?;

    Ok(Outcome::Run(Box::new(Args {
        task,
        sessions,
        model_given,
        provider_given,
        json,
        inline,
        worker,
        commands,
        listen,
        serve,
        model: model_id,
        provider,
        profile,
        base_url,
        compat,
        api_key: table.api_key,
        provider_name: provider_name.map(str::to_string),
        preset: built_in,
        auth: route.map_or(Auth::Wire, |route| route.auth),
        login: provider_name.map(|name| table.login.clone().unwrap_or_else(|| name.to_string())),
        api_key_env: api_key_env
            .or(table.api_key_env)
            .or_else(|| built_in.map(|preset| preset.api_key_env.to_string())),
        header,
        max_tokens: max_tokens
            .or(model_table.max_tokens)
            .or(settings.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS),
        // An empty value in a config table counts as unset.
        reasoning: reasoning.or_else(|| {
            [model_table.reasoning, table.reasoning, settings.reasoning]
                .into_iter()
                .flatten()
                .find(|text| !text.trim().is_empty())
        }),
        deadline,
        codemode: settings.codemode,
        session: settings.session,
        tools,
        mcp_servers,
        warn_many_direct: servers.warn_many_direct,
        cwd,
        policy,
        skip: reagent_config::resolve_list(&settings.skip, &[]),
        bash_reads: settings.bash_reads.unwrap_or(true),
        rules,
        context_window: model_table.context_window,
        price: model_table.price,
        sandbox,
        network,
        write,
        plugins_dir,
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
    use reagent_config::McpTransport;

    fn parse(args: &[&str]) -> Result<Outcome, String> {
        parse_with(args, Settings::default())
    }

    fn parse_with(args: &[&str], settings: Settings) -> Result<Outcome, String> {
        parse_with_servers(args, settings, BTreeMap::new())
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

    #[test]
    fn bash_reads_comes_from_the_config() {
        assert!(run_with(&["-p", "x"], Settings::default()).bash_reads);
        let off = Settings {
            bash_reads: Some(false),
            ..Settings::default()
        };
        assert!(!run_with(&["-p", "x"], off).bash_reads);
    }

    /// `--inline` asks for the inline UI, and is off unless it is given.
    #[test]
    fn the_inline_flag_asks_for_the_inline_ui() {
        assert!(!run_with(&["-p", "x"], Settings::default()).inline);
        assert!(run_with(&["-p", "x", "--inline"], Settings::default()).inline);
    }

    #[test]
    fn edit_flag_is_unknown() {
        let error = parse_with(&["-p", "x", "--edit", "lines"], Settings::default()).unwrap_err();
        assert!(
            error.starts_with("unknown argument") && error.contains("edit"),
            "{error}"
        );
    }

    fn providers(provider: Provider) -> std::collections::BTreeMap<String, Provider> {
        [("fi".to_string(), provider)].into()
    }

    /// The settings of a config with one table per `(name, preset, login)`.
    fn account_settings(tables: &[(&str, Option<&str>, Option<&str>)]) -> Settings {
        let providers = tables
            .iter()
            .map(|(name, preset, login)| {
                (
                    name.to_string(),
                    Provider {
                        preset: preset.map(str::to_string),
                        login: login.map(str::to_string),
                        ..Provider::default()
                    },
                )
            })
            .collect();
        Settings {
            providers,
            ..Settings::default()
        }
    }

    #[test]
    fn named_account_tables_resolve_their_own_preset_wire_and_compat() {
        let settings = account_settings(&[
            ("codex-sub", Some("chatgpt"), None),
            ("codex-api", Some("openai"), None),
        ]);
        let sub = run_with(
            &["-p", "task", "--model", "codex-sub/gpt-x"],
            settings.clone(),
        );
        assert_eq!(sub.model, "gpt-x");
        assert_eq!(sub.provider, Wire::OpenAiResponses);
        assert_eq!(sub.base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(sub.login.as_deref(), Some("codex-sub"));
        assert_eq!(sub.preset.map(|preset| preset.name), Some("chatgpt"));
        assert_eq!(
            sub.compat.map(|compat| compat.max_tokens_field),
            Some(MaxTokensField::Omit)
        );
        let api = run_with(&["-p", "task", "--model", "codex-api/gpt-x"], settings);
        assert_eq!(api.provider, Wire::OpenAiResponses);
        assert_eq!(api.preset.map(|preset| preset.name), Some("openai"));
        assert_eq!(
            api.compat.map(|compat| compat.max_tokens_field),
            Some(MaxTokensField::MaxCompletionTokens)
        );
    }

    #[test]
    fn named_account_a_table_login_names_the_saved_account() {
        let settings = account_settings(&[("codex-api", Some("openai"), Some("codex-sub"))]);
        let args = run_with(&["-p", "task", "--model", "codex-api/gpt-x"], settings);
        assert_eq!(args.login.as_deref(), Some("codex-sub"));
    }

    #[test]
    fn named_account_bad_names_are_refused() {
        for name in ["codex/sub", "a.b"] {
            let settings = account_settings(&[(name, Some("openai"), None)]);
            let error = parse_with(&["-p", "task"], settings).unwrap_err();
            assert!(error.contains("provider name"), "{error}");
        }
        let settings = account_settings(&[("codex-api", Some("openai"), Some("a.b"))]);
        let error = parse_with(&["models"], settings).unwrap_err();
        assert!(error.contains("provider name a.b"), "{error}");
    }

    #[test]
    fn named_account_a_leading_dash_is_refused() {
        assert!(check_account_name("--no-browser").is_err());
        assert!(check_account_name("-").is_err());
        assert!(check_account_name("or-work").is_ok());
    }

    #[test]
    fn named_account_unknown_preset_is_refused() {
        let settings = account_settings(&[("codex-sub", Some("nope"), None)]);
        let error = parse_with(&["-p", "task"], settings).unwrap_err();
        assert!(
            error.contains("config: [providers.codex-sub]: unknown preset nope"),
            "{error}"
        );
        assert!(error.contains("presets: anthropic"), "{error}");
    }

    fn run(args: &[&str]) -> Args {
        match parse(args) {
            Ok(Outcome::Run(args)) => *args,
            other => panic!("expected a run outcome for {args:?}, got {other:?}"),
        }
    }

    #[test]
    fn policy_default_is_budget_and_snapshot() {
        assert_eq!(run(&["-p", "task"]).policy, vec!["budget", "snapshot"]);
    }

    #[test]
    fn policy_default_is_replaced_by_a_flag() {
        assert_eq!(
            run(&["-p", "task", "--policy", "stuck"]).policy,
            vec!["stuck"]
        );
    }

    #[test]
    fn policy_default_on_the_chat_wire_is_budget_mask_and_snapshot() {
        let args = run(&["-p", "task", "--provider", "openai-chat"]);
        assert_eq!(args.policy, vec!["budget", "mask", "snapshot"]);
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
    fn snapshot_policy_is_on_by_default_on_both_wires() {
        let args = run(&["-p", "task"]);
        assert!(
            args.policy.iter().any(|name| name == "snapshot"),
            "{args:?}"
        );
        let args = run(&["-p", "task", "--provider", "openai-chat"]);
        assert!(
            args.policy.iter().any(|name| name == "snapshot"),
            "{args:?}"
        );
    }

    #[test]
    fn snapshot_policy_is_left_out_by_a_flag() {
        let args = run(&["-p", "task", "--policy", "budget"]);
        assert!(
            !args.policy.iter().any(|name| name == "snapshot"),
            "{args:?}"
        );
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
    fn compact_policy_runs_on_every_wire() {
        for wire in [
            "anthropic",
            "openai-chat",
            "openai-responses",
            "pi-messages",
        ] {
            let args = parse(&["-p", "task", "--policy", "compact", "--provider", wire])
                .unwrap_or_else(|error| panic!("{wire}: {error}"));
            match args {
                Outcome::Run(args) => assert_eq!(args.policy, vec!["compact"], "{wire}"),
                other => panic!("expected a run outcome for {wire}, got {other:?}"),
            }
        }
        assert_eq!(
            run(&["-p", "task", "--policy", "compact"]).policy,
            vec!["compact"]
        );
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

    /// The merged settings of the three config layers.
    fn merged(global: Option<Layer>, profile: Option<Layer>, repo: Option<Layer>) -> Settings {
        reagent_config::merge_all(&Layers {
            global,
            profile,
            repo,
            saved: None,
        })
    }

    fn layer(path: &str, settings: Settings) -> Layer {
        Layer {
            path: PathBuf::from(path),
            settings,
        }
    }

    #[test]
    fn ask_the_default_tools_hold_it_and_a_minus_never_does() {
        let args = run(&["-p", "task"]);
        assert!(
            args.tools.iter().any(|name| name == "ask"),
            "{:?}",
            args.tools
        );
        let args = run(&["-p", "task", "--tools", "-ask"]);
        assert!(
            !args.tools.iter().any(|name| name == "ask"),
            "{:?}",
            args.tools
        );
        let args = run(&["-p", "task", "--tools", "read,bash"]);
        assert!(
            !args.tools.iter().any(|name| name == "ask"),
            "{:?}",
            args.tools
        );
        let args = run(&["-p", "task", "--tools", "+ask"]);
        assert!(
            args.tools.iter().any(|name| name == "ask"),
            "{:?}",
            args.tools
        );
    }

    #[test]
    fn list_edit_tools_add_and_remove() {
        let args = run(&["-p", "task", "--tools", "+codemode,-write"]);
        assert_eq!(
            args.tools,
            ["ask", "bash", "codemode", "edit", "find", "grep", "read"]
        );
    }

    #[test]
    fn list_edit_tools_plain_replaces() {
        let args = run(&["-p", "task", "--tools", "read,grep"]);
        assert_eq!(args.tools, ["grep", "read"]);
        let args = run(&["-p", "task", "-t", "bash"]);
        assert_eq!(args.tools, ["bash"]);
    }

    // The MCP tests below build their servers instead of reading `mcp.toml`.
    fn server(enabled: bool) -> McpServer {
        McpServer {
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: None,
            },
            timeout: std::time::Duration::from_secs(60),
            enabled,
            direct: McpDirect::None,
            oauth: None,
        }
    }

    /// The servers of a fake `mcp.toml`, by name and whether they are enabled.
    fn defined(list: &[(&str, bool)]) -> BTreeMap<String, McpServer> {
        list.iter()
            .map(|(name, enabled)| ((*name).to_string(), server(*enabled)))
            .collect()
    }

    fn parse_with_servers(
        args: &[&str],
        settings: Settings,
        servers: BTreeMap<String, McpServer>,
    ) -> Result<Outcome, String> {
        let argv = args.iter().map(std::ffi::OsString::from);
        parse_from(
            lexopt::Parser::from_args(argv),
            &|_, _| {
                Ok((
                    settings.clone(),
                    Rules::none(),
                    McpConfig {
                        servers: servers.clone(),
                        warn_many_direct: true,
                        settings: Default::default(),
                    },
                ))
            },
            false,
            false,
        )
    }

    /// Parses `args` as a default build does, where a run with no task may
    /// open the terminal UI.
    fn parse_ui(args: &[&str]) -> Result<Outcome, String> {
        let argv = args.iter().map(std::ffi::OsString::from);
        parse_from(
            lexopt::Parser::from_args(argv),
            &|_, _| {
                Ok((
                    Settings::default(),
                    Rules::none(),
                    McpConfig {
                        servers: BTreeMap::new(),
                        warn_many_direct: true,
                        settings: Default::default(),
                    },
                ))
            },
            false,
            true,
        )
    }

    /// Parses `args` as a run the worker word started does.
    fn parse_worker(args: &[&str]) -> Result<Outcome, String> {
        let argv = args.iter().map(std::ffi::OsString::from);
        parse_from(
            lexopt::Parser::from_args(argv),
            &|_, _| {
                Ok((
                    Settings::default(),
                    Rules::none(),
                    McpConfig {
                        servers: BTreeMap::new(),
                        warn_many_direct: true,
                        settings: Default::default(),
                    },
                ))
            },
            true,
            false,
        )
    }

    /// The UI opens only a new session or a continue, so a run that can do
    /// neither still needs its task, in a build with the UI too.
    #[test]
    fn no_session_and_fork_still_need_a_task_when_the_ui_is_built() {
        for args in [&["--no-session"][..], &["--fork", "abc"][..]] {
            match parse_ui(args) {
                Err(message) => assert!(message.contains("missing task"), "{args:?}: {message}"),
                Ok(outcome) => panic!("{args:?} parsed with no task: {outcome:?}"),
            }
        }
        assert!(
            matches!(parse_ui(&[]), Ok(Outcome::Run(_))),
            "a plain run opens the UI"
        );
    }

    /// The message that refused `outcome`, which names `flag`.
    fn refused(outcome: Result<Outcome, String>, flag: &str) -> String {
        match outcome {
            Err(message) => {
                assert!(message.contains(flag), "{message} names no {flag}");
                message
            }
            Ok(outcome) => panic!("{flag} parsed: {outcome:?}"),
        }
    }

    #[test]
    fn steer_flags_commands_needs_json() {
        let message = refused(parse(&["--commands", "-p", "x"]), "--commands");
        assert!(message.contains("--json"), "{message}");
    }

    #[test]
    fn steer_flags_commands_refuses_the_task_from_stdin() {
        let message = refused(parse(&["--json", "--commands", "-p", "-"]), "--commands");
        assert!(message.contains("stdin"), "{message}");
    }

    #[test]
    fn steer_flags_refuse_no_session() {
        for flag in ["--commands", "--listen"] {
            let args: &[&str] = if flag == "--commands" {
                &["--json", "--no-session", "--commands", "-p", "x"]
            } else {
                &["--no-session", "--listen", "-p", "x"]
            };
            let message = refused(parse(args), "--no-session");
            assert!(message.contains(flag), "{message} names no {flag}");
        }
    }

    #[test]
    fn steer_flags_refuse_the_worker_word() {
        let message = refused(
            parse_worker(&["--json", "--commands", "-p", "x"]),
            "--commands",
        );
        assert!(message.contains("worker"), "{message}");
        let message = refused(parse_worker(&["--listen", "-p", "x"]), "--listen");
        assert!(message.contains("worker"), "{message}");
    }

    #[test]
    fn steer_flags_commands_runs_with_no_task() {
        let args = run_with(&["--json", "--commands"], Settings::default());
        assert!(args.commands);
        assert!(args.task.is_none(), "no task is queued");
    }

    #[test]
    fn steer_flags_listen_parses_with_a_task() {
        let args = run_with(&["--listen", "-p", "x"], Settings::default());
        assert!(args.listen);
        assert!(!args.commands);
    }

    #[test]
    fn steer_flags_neither_is_on_by_default() {
        let args = run_with(&["-p", "x"], Settings::default());
        assert!(!args.commands);
        assert!(!args.listen);
    }

    /// A `-p` run with a session serves its socket unless a flag says otherwise.
    #[test]
    fn serve_is_on_for_a_print_run_with_a_session() {
        let args = run_with(&["-p", "x"], Settings::default());
        assert!(args.serve, "a -p run serves its socket by default");
        assert!(args.task.is_some());
        assert!(!matches!(args.sessions, Sessions::None));
    }

    #[test]
    fn serve_is_off_for_the_flags_that_take_clients_another_way() {
        for args in [
            &["--no-serve", "-p", "x"][..],
            &["--no-session", "-p", "x"][..],
            &["--listen", "-p", "x"][..],
            &["--json", "--commands", "-p", "x"][..],
        ] {
            assert!(!run_with(args, Settings::default()).serve, "{args:?}");
        }
        // A run with no task is no `-p` run, and a worker serves already.
        assert!(!run_with(&["--json", "--commands"], Settings::default()).serve);
        match parse_worker(&["-p", "x"]) {
            Ok(Outcome::Run(args)) => assert!(!args.serve, "a worker serves already"),
            other => panic!("expected a worker run, got {other:?}"),
        }
    }

    #[test]
    fn serve_refuses_the_flags_that_take_clients() {
        let message = refused(parse(&["--no-serve", "--listen", "-p", "x"]), "--no-serve");
        assert!(message.contains("--listen"), "{message}");
        let message = refused(
            parse(&["--no-serve", "--json", "--commands", "-p", "x"]),
            "--no-serve",
        );
        assert!(message.contains("--commands"), "{message}");
        let message = refused(parse_worker(&["--no-serve", "-p", "x"]), "--no-serve");
        assert!(message.contains("--worker"), "{message}");
    }

    fn run_servers(
        args: &[&str],
        settings: Settings,
        servers: BTreeMap<String, McpServer>,
    ) -> Args {
        match parse_with_servers(args, settings, servers) {
            Ok(Outcome::Run(args)) => *args,
            other => panic!("expected a run outcome for {args:?}, got {other:?}"),
        }
    }

    fn picked(args: &Args) -> Vec<&str> {
        args.mcp_servers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    #[test]
    fn mcp_config_default_picks_enabled() {
        let servers = defined(&[("a", true), ("b", false)]);
        let args = run_servers(&["-p", "task"], Settings::default(), servers);
        assert_eq!(picked(&args), ["a"]);
        assert!(args.mcp_servers[0].1.enabled, "the entry reaches the args");
    }

    #[test]
    fn mcp_codemode_turns_on_with_a_server() {
        let servers = defined(&[("gh", true)]);
        let args = run_servers(&["-p", "task"], Settings::default(), servers);
        assert_eq!(picked(&args), ["gh"]);
        assert!(
            args.tools.iter().any(|name| name == "codemode"),
            "{:?}",
            args.tools
        );
        // An edit layer that says nothing about codemode keeps it too.
        let servers = defined(&[("gh", true)]);
        let args = run_servers(
            &["-p", "task", "--tools", "+find"],
            Settings::default(),
            servers,
        );
        assert!(args.tools.iter().any(|name| name == "codemode"));
    }

    #[test]
    fn mcp_codemode_off_when_removed() {
        let servers = defined(&[("gh", true)]);
        let args = run_servers(
            &["-p", "task", "--tools", "-codemode"],
            Settings::default(),
            servers,
        );
        assert!(args.mcp_servers.is_empty(), "{:?}", picked(&args));
        assert!(!args.tools.iter().any(|name| name == "codemode"));
        let servers = defined(&[("gh", true)]);
        let args = run_servers(
            &["-p", "task", "--tools", "read,grep"],
            Settings::default(),
            servers,
        );
        assert!(args.mcp_servers.is_empty(), "{:?}", picked(&args));
        // A plain list that keeps codemode keeps the servers.
        let servers = defined(&[("gh", true)]);
        let args = run_servers(
            &["-p", "task", "--tools", "codemode,read"],
            Settings::default(),
            servers,
        );
        assert_eq!(picked(&args), ["gh"]);
    }

    #[test]
    fn mcp_direct_codemode_needed_for_rest() {
        let mut servers = defined(&[("gh", true)]);
        servers.get_mut("gh").expect("the server").direct =
            McpDirect::Only(vec!["echo".to_string()]);
        let args = run_servers(&["-p", "task"], Settings::default(), servers);
        assert_eq!(picked(&args), ["gh"]);
        assert!(
            args.tools.iter().any(|name| name == "codemode"),
            "{:?}",
            args.tools
        );
    }

    #[test]
    fn mcp_config_repo_removes_one() {
        let servers = defined(&[("gh", true), ("slack", true)]);
        let settings = merged(
            Some(layer("/g/config.toml", Settings::default())),
            None,
            Some(layer(
                "/repo/.reagent/config.toml",
                Settings {
                    mcp: Some(vec!["-slack".into()]),
                    ..Settings::default()
                },
            )),
        );
        let args = run_servers(&["-p", "task"], settings, servers);
        assert_eq!(picked(&args), ["gh"]);
    }

    #[test]
    fn mcp_config_repo_cannot_enable_a_disabled_server() {
        let servers = defined(&[("dev", true), ("prod", false)]);
        for list in [
            vec!["+prod".to_string()],
            vec!["prod".to_string()],
            vec!["+prod".to_string(), "+dev".to_string()],
        ] {
            let settings = settings_of(
                None,
                Some(layer(
                    "/repo/.reagent/config.toml",
                    Settings {
                        mcp: Some(list.clone()),
                        ..Settings::default()
                    },
                )),
                &servers,
            );
            let args = run_servers(&["-p", "task"], settings, servers.clone());
            assert!(!picked(&args).contains(&"prod"), "{list:?}");
        }
        // The global file and the folder's list keep the full rules.
        let settings = settings_of(
            Some(layer(
                "/g/config.toml",
                Settings {
                    mcp: Some(vec!["+prod".into()]),
                    ..Settings::default()
                },
            )),
            None,
            &servers,
        );
        let args = run_servers(&["-p", "task"], settings, servers.clone());
        assert!(picked(&args).contains(&"prod"), "{:?}", picked(&args));
        let args = run_servers(
            &["-p", "task", "--mcp", "+prod"],
            Settings::default(),
            servers,
        );
        assert!(picked(&args).contains(&"prod"), "{:?}", picked(&args));
    }

    #[test]
    fn mcp_config_repo_message_names_the_file_the_server_and_the_flag() {
        assert_eq!(
            repo_disabled(Path::new("/repo/.reagent/config.toml"), "prod"),
            concat!(
                "reagent: /repo/.reagent/config.toml asks for MCP server prod, which is disabled; ",
                "enable it in mcp.toml or pass --mcp +prod"
            )
        );
    }

    /// The settings the three files merge to, with the repo file's `mcp` list
    /// cut down to what a repo file may start.
    fn settings_of(
        global: Option<Layer>,
        repo: Option<Layer>,
        servers: &BTreeMap<String, McpServer>,
    ) -> Settings {
        let layers = Layers {
            global,
            profile: None,
            repo,
            saved: None,
        };
        settings_from(layers, Path::new("/repo"), servers)
            .expect("settings")
            .0
    }

    #[test]
    fn mcp_config_flag_adds_disabled() {
        let servers = defined(&[("a", true), ("b", false)]);
        let args = run_servers(&["-p", "task", "--mcp", "+b"], Settings::default(), servers);
        assert_eq!(picked(&args), ["a", "b"]);
        assert!(!args.mcp_servers[1].1.enabled);
    }

    #[test]
    fn mcp_config_no_mcp() {
        let servers = defined(&[("a", true)]);
        let args = run_servers(
            &["-p", "task", "--mcp", "+a", "--no-mcp"],
            Settings::default(),
            servers,
        );
        assert!(args.mcp_servers.is_empty(), "{:?}", picked(&args));
    }

    #[test]
    fn mcp_config_unknown_name() {
        let servers = defined(&[("a", true), ("b", false)]);
        let error = parse_with_servers(
            &["-p", "task", "--mcp", "+nope"],
            Settings::default(),
            servers,
        )
        .unwrap_err();
        assert!(error.contains("nope"), "{error}");
        assert!(error.contains("a, b"), "{error}");
    }

    #[test]
    fn list_edit_mixed_list_is_an_error() {
        let error = parse(&["-p", "task", "--tools", "read,+grep"]).unwrap_err();
        assert!(error.contains("not both"), "{error}");
        let error = parse(&["-p", "task", "--policy", "budget,+stuck"]).unwrap_err();
        assert!(error.contains("not both"), "{error}");
    }

    #[test]
    fn list_edit_unknown_tool_is_named() {
        let error = parse(&["-p", "task", "--tools", "+nope"]).unwrap_err();
        assert!(error.contains("nope"), "{error}");
        for name in ["bash", "edit", "find", "grep", "read", "write", "codemode"] {
            assert!(error.contains(name), "{error}");
        }
        let error = parse(&["-p", "task", "--policy", "+nope"]).unwrap_err();
        assert!(error.contains("nope"), "{error}");
        for word in ["all", "none"] {
            for sign in ["+", "-"] {
                assert!(
                    parse(&["-p", "task", "--policy", &format!("{sign}{word}")]).is_err(),
                    "{sign}{word}"
                );
            }
        }
    }

    #[test]
    fn list_edit_policy_layers_stack() {
        let settings = merged(
            Some(layer(
                "/g/config.toml",
                Settings {
                    policy: Some(vec!["+stuck".into()]),
                    ..Settings::default()
                },
            )),
            Some(layer(
                "/g/profiles/p.toml",
                Settings {
                    policy: Some(vec!["-snapshot".into()]),
                    ..Settings::default()
                },
            )),
            None,
        );
        assert_eq!(
            run_with(&["-p", "task"], settings).policy,
            ["budget", "stuck"]
        );
    }

    #[test]
    fn list_edit_policy_flag_goes_last() {
        let settings = Settings {
            policy: Some(vec!["+stuck".into()]),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task", "--policy", "-stuck"], settings);
        assert_eq!(args.policy, ["budget", "snapshot"]);
    }

    #[test]
    fn list_edit_policy_plain_config_then_flag_edit() {
        let settings = Settings {
            policy: Some(vec!["completion".into()]),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task", "--policy", "+stuck"], settings);
        assert_eq!(args.policy, ["completion", "stuck"]);
    }

    #[test]
    fn list_edit_policy_chat_default_edit() {
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "-mask",
        ]);
        assert_eq!(args.policy, ["budget", "snapshot"]);
    }

    #[test]
    fn list_edit_skip_removes() {
        let settings = merged(
            Some(layer(
                "/g/config.toml",
                Settings {
                    skip: vec!["vendor".into()],
                    ..Settings::default()
                },
            )),
            None,
            Some(layer(
                "/w/.reagent/config.toml",
                Settings {
                    skip: vec!["-vendor".into()],
                    ..Settings::default()
                },
            )),
        );
        assert!(run_with(&["-p", "task"], settings).skip.is_empty());
    }

    #[test]
    fn list_edit_empty_list_selects_nothing() {
        let empty = |path: &str| {
            Some(layer(
                path,
                Settings {
                    policy: Some(Vec::new()),
                    tools: Some(Vec::new()),
                    ..Settings::default()
                },
            ))
        };
        let args = run_with(&["-p", "task"], merged(empty("/g/config.toml"), None, None));
        assert!(args.policy.is_empty(), "{:?}", args.policy);
        assert!(args.tools.is_empty(), "{:?}", args.tools);
        let edited = run_with(
            &["-p", "task", "--tools", "+read"],
            merged(empty("/g/config.toml"), None, None),
        );
        assert_eq!(edited.tools, ["read"]);
    }

    #[test]
    fn codemode_table_wall_time() {
        let settings = Settings {
            codemode: reagent_config::CodemodeSettings {
                wall_time: Some(30),
            },
            ..Settings::default()
        };
        let args = run_with(&["-p", "task"], settings.clone());
        assert_eq!(
            crate::run::codemode_wall(args.codemode.wall_time, args.deadline),
            std::time::Duration::from_secs(30)
        );
        let args = run_with(&["-p", "task", "--deadline", "10"], settings);
        assert_eq!(
            crate::run::codemode_wall(args.codemode.wall_time, args.deadline),
            std::time::Duration::from_secs(10)
        );
        let args = run_with(&["-p", "task"], Settings::default());
        assert_eq!(
            crate::run::codemode_wall(args.codemode.wall_time, args.deadline),
            std::time::Duration::from_secs(600)
        );
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
        let error = check_lists(&mut layer).unwrap_err();
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
        check_lists(&mut layer).unwrap();
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
        let (_, rules) = settings_from(
            Layers {
                global: Some(layer),
                ..Layers::default()
            },
            Path::new("/work"),
            &BTreeMap::new(),
        )
        .unwrap();
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
        let error = settings_from(
            Layers {
                global: Some(bad),
                ..Layers::default()
            },
            Path::new("/work"),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.starts_with("/g/config.toml"), "{error}");
    }

    #[test]
    fn profile_layer_the_flag_beats_the_env_var() {
        let some = |name: &str| Some(name.to_string());
        assert_eq!(profile_from(some("work"), some("env")), some("work"));
        assert_eq!(profile_from(None, some("env")), some("env"));
        assert_eq!(profile_from(None, Some(String::new())), None);
        assert_eq!(profile_from(None, None), None);
    }

    #[test]
    fn profile_layer_a_bad_name_is_refused() {
        for name in ["a/b", "a.b", "-work", ""] {
            let error = parse(&["--profile", name, "-p", "task"]).unwrap_err();
            assert!(error.contains("invalid profile"), "{name}: {error}");
        }
        assert!(check_profile_name("work-1_x").is_ok());
    }

    #[test]
    fn profile_layer_reaches_the_loader_of_both_commands() {
        let seen = std::cell::RefCell::new(Vec::new());
        let parse = |args: &[&str]| {
            let argv = args.iter().map(std::ffi::OsString::from);
            parse_from(
                lexopt::Parser::from_args(argv),
                &|_: &Path, profile: Option<&str>| {
                    seen.borrow_mut().push(profile.map(str::to_string));
                    Ok((
                        Settings::default(),
                        Rules::none(),
                        McpConfig {
                            servers: BTreeMap::new(),
                            warn_many_direct: true,
                            settings: Default::default(),
                        },
                    ))
                },
                false,
                false,
            )
        };
        let models = parse(&["models", "--profile", "work"]).expect("parses");
        match models {
            Outcome::Models(models) => assert_eq!(models.profile.as_deref(), Some("work")),
            other => panic!("expected the models command, got {other:?}"),
        }
        let run = parse(&["-p", "task", "--profile", "work"]).expect("parses");
        match run {
            Outcome::Run(args) => assert_eq!(args.profile.as_deref(), Some("work")),
            other => panic!("expected a run outcome, got {other:?}"),
        }
        assert_eq!(
            seen.borrow().as_slice(),
            [Some("work".into()), Some("work".into())]
        );
    }

    #[test]
    fn profile_layer_the_models_command_refuses_a_run_flag() {
        let error = parse(&["models", "--profile", "work", "--model", "x"]).unwrap_err();
        assert_eq!(error, "the models command takes no run flags");
    }

    #[test]
    fn profile_layer_a_provider_table_from_the_profile_reaches_the_args() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = Layer {
            path: dir.path().join("config.toml"),
            settings: Settings {
                model: Some("fi/glm".into()),
                ..Settings::default()
            },
        };
        let profile = Layer {
            path: dir.path().join("profiles/work.toml"),
            settings: Settings {
                providers: providers(Provider {
                    api_key_env: Some("WORK_KEY".into()),
                    ..Provider::default()
                }),
                ..Settings::default()
            },
        };
        let layers = Layers {
            global: Some(global),
            profile: Some(profile),
            repo: None,
            saved: None,
        };
        let (settings, _) = settings_from(layers, dir.path(), &BTreeMap::new()).expect("settings");
        let args = run_with(&["-p", "task"], settings.clone());
        assert_eq!(args.model, "glm");
        assert_eq!(args.api_key_env.as_deref(), Some("WORK_KEY"));
        // The model flag still beats the profile's model.
        let args = run_with(&["-p", "task", "--model", "other"], settings);
        assert_eq!(args.model, "other");
    }

    #[test]
    fn a_config_error_ends_the_parse() {
        let argv = ["-p", "task"].iter().map(std::ffi::OsString::from);
        let error = parse_from(
            lexopt::Parser::from_args(argv),
            &|_, _| Err("bad".into()),
            false,
            false,
        );
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
        assert_eq!(
            names,
            [
                "fi",
                "anthropic",
                "openai",
                "chatgpt",
                "openrouter",
                "opencode-go",
                "commandcode",
                "deepseek",
                "freeinference",
                "radius",
                "synthetic"
            ]
        );
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
        assert_eq!(args.policy, vec!["budget", "mask", "snapshot"]);
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
                        ..Model::default()
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
    fn reasoning_text_empty_in_a_table_counts_as_unset() {
        let settings = Settings {
            model: Some("fi/glm".into()),
            reasoning: Some("top".into()),
            providers: providers(Provider {
                wire: Some("openai-chat".into()),
                reasoning: Some("  ".into()),
                ..Provider::default()
            }),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task"], settings);
        assert_eq!(args.reasoning.as_deref(), Some("top"));
    }

    #[test]
    fn reasoning_text_resolves_flag_model_provider_then_top_level() {
        let settings = Settings {
            model: Some("fi/glm".into()),
            reasoning: Some("top".into()),
            providers: providers(Provider {
                wire: Some("openai-chat".into()),
                reasoning: Some("provider".into()),
                models: [(
                    "glm".to_string(),
                    Model {
                        reasoning: Some("model".into()),
                        ..Model::default()
                    },
                )]
                .into(),
                ..Provider::default()
            }),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task"], settings.clone());
        assert_eq!(args.reasoning.as_deref(), Some("model"));

        let args = run_with(&["-p", "task", "--reasoning", "xhigh"], settings.clone());
        assert_eq!(args.reasoning.as_deref(), Some("xhigh"));

        let mut provider_only = settings.clone();
        provider_only
            .providers
            .get_mut("fi")
            .expect("the provider table")
            .models
            .clear();
        let args = run_with(&["-p", "task"], provider_only);
        assert_eq!(args.reasoning.as_deref(), Some("provider"));

        let mut top_only = settings;
        let provider = top_only
            .providers
            .get_mut("fi")
            .expect("the provider table");
        provider.reasoning = None;
        provider.models.clear();
        let args = run_with(&["-p", "task"], top_only);
        assert_eq!(args.reasoning.as_deref(), Some("top"));
    }

    #[test]
    fn the_reasoning_text_flag_needs_a_value() {
        assert_eq!(run(&["-p", "task"]).reasoning, None);
        assert_eq!(
            run(&["-p", "task", "--reasoning", " 4096 "])
                .reasoning
                .as_deref(),
            Some("4096")
        );
        assert!(parse(&["-p", "task", "--reasoning"]).is_err());
        assert!(parse(&["-p", "task", "--reasoning", " "]).is_err());
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

    #[test]
    fn preset_name_selects_its_wire_url_and_key_env() {
        let args = run(&["-p", "task", "--model", "opencode-go/glm-5"]);
        assert_eq!(args.model, "glm-5");
        assert_eq!(args.provider, Wire::OpenAiChat);
        assert_eq!(
            args.base_url.as_deref(),
            Some("https://opencode.ai/zen/go/v1")
        );
        assert_eq!(args.api_key_env.as_deref(), Some("OPENCODE_API_KEY"));
        assert_eq!(args.policy, vec!["budget", "mask", "snapshot"]);
    }

    fn table(name: &str, provider: Provider) -> Settings {
        Settings {
            providers: [(name.to_string(), provider)].into(),
            ..Settings::default()
        }
    }

    #[test]
    fn route_a_commandcode_claude_model_takes_the_messages_endpoint() {
        let args = run(&["-p", "task", "--model", "commandcode/claude-haiku-5-5"]);
        assert_eq!(args.model, "claude-haiku-5-5");
        assert_eq!(args.provider, Wire::Anthropic);
        assert_eq!(
            args.base_url.as_deref(),
            Some("https://api.commandcode.ai/provider")
        );
        assert_eq!(args.auth, Auth::Wire);
        assert_eq!(args.api_key_env.as_deref(), Some("COMMANDCODE_API_KEY"));
        // The chat wire's default mask does not apply on Messages.
        assert_eq!(args.policy, vec!["budget", "snapshot"]);

        let other = run(&[
            "-p",
            "task",
            "--model",
            "commandcode/deepseek/deepseek-v4.1-flash",
        ]);
        assert_eq!(other.provider, Wire::OpenAiChat);
        assert_eq!(
            other.base_url.as_deref(),
            Some("https://api.commandcode.ai/provider/v1")
        );
        assert_eq!(other.compat.expect("a compat").reasoning_off, Some("off"));
    }

    #[test]
    fn route_a_config_endpoint_wins_over_the_route_endpoint() {
        let settings = table(
            "commandcode",
            Provider {
                base_url: Some("http://localhost:1".into()),
                ..Provider::default()
            },
        );
        let args = run_with(
            &["-p", "task", "--model", "commandcode/claude-haiku-5-5"],
            settings,
        );
        assert_eq!(args.provider, Wire::Anthropic);
        assert_eq!(args.base_url.as_deref(), Some("http://localhost:1"));
    }

    #[test]
    fn route_a_named_wire_picks_its_route() {
        let deepseek = run_with(
            &["-p", "task", "--model", "deepseek/deepseek-flash"],
            table(
                "deepseek",
                Provider {
                    wire: Some("anthropic".into()),
                    ..Provider::default()
                },
            ),
        );
        assert_eq!(deepseek.provider, Wire::Anthropic);
        assert_eq!(
            deepseek.base_url.as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );

        let chat = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--model",
            "commandcode/claude-haiku-5-5",
        ]);
        assert_eq!(chat.provider, Wire::OpenAiChat);
        assert_eq!(
            chat.base_url.as_deref(),
            Some("https://api.commandcode.ai/provider/v1")
        );

        let openrouter = run_with(
            &[
                "-p",
                "task",
                "--model",
                "openrouter/anthropic/claude-sonnet-5",
            ],
            table(
                "openrouter",
                Provider {
                    wire: Some("anthropic".into()),
                    ..Provider::default()
                },
            ),
        );
        assert_eq!(
            openrouter.base_url.as_deref(),
            Some("https://openrouter.ai/api")
        );
        assert_eq!(openrouter.auth, Auth::Bearer);
    }

    #[test]
    fn route_a_named_wire_with_no_route_is_an_error() {
        let error = parse(&[
            "-p",
            "task",
            "--provider",
            "openai-responses",
            "--model",
            "deepseek/deepseek-flash",
        ])
        .unwrap_err();
        assert!(error.contains("no openai-responses route"), "{error}");
        assert!(
            error.contains("chat (openai-chat), messages (anthropic)"),
            "{error}"
        );
        // An endpoint of its own needs no route.
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-responses",
            "--base-url",
            "http://localhost:1/v1",
            "--model",
            "deepseek/deepseek-flash",
        ]);
        assert_eq!(args.provider, Wire::OpenAiResponses);
        assert_eq!(args.auth, Auth::Wire);
    }

    #[test]
    fn route_an_own_endpoint_with_no_route_keeps_the_host_rule() {
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-responses",
            "--base-url",
            "https://api.openai.com/v1",
            "--model",
            "deepseek/deepseek-flash",
        ]);
        assert_eq!(
            args.compat.expect("a compat").max_tokens_field,
            MaxTokensField::MaxCompletionTokens
        );
    }

    #[test]
    fn route_the_model_list_checks_a_wire_it_does_not_use() {
        let error = model_targets(
            &table(
                "deepseek",
                Provider {
                    wire: Some("nope".into()),
                    ..Provider::default()
                },
            ),
            Some("deepseek"),
        )
        .expect_err("a bad wire is an error");
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn route_the_model_list_reads_the_default_route() {
        let targets = model_targets(
            &table(
                "deepseek",
                Provider {
                    wire: Some("anthropic".into()),
                    ..Provider::default()
                },
            ),
            Some("deepseek"),
        )
        .expect("the targets");
        assert_eq!(targets[0].wire, Wire::OpenAiChat);
        assert_eq!(
            targets[0].base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
    }

    #[test]
    fn a_config_table_overrides_one_preset_field() {
        let settings = Settings {
            providers: [(
                "openrouter".to_string(),
                Provider {
                    base_url: Some("http://localhost:1/v1".into()),
                    ..Provider::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task", "--model", "openrouter/x"], settings);
        assert_eq!(args.provider, Wire::OpenAiChat);
        assert_eq!(args.base_url.as_deref(), Some("http://localhost:1/v1"));
        assert_eq!(args.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    }

    #[test]
    fn preset_compat_applies_only_to_the_preset_endpoint() {
        let settings = |base_url: Option<&str>| Settings {
            providers: [(
                "openai".to_string(),
                Provider {
                    wire: Some("openai-chat".into()),
                    base_url: base_url.map(str::to_owned),
                    ..Provider::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        let field = |base_url| {
            run_with(&["-p", "task", "--model", "openai/x"], settings(base_url))
                .compat
                .expect("a compat")
                .max_tokens_field
        };
        assert_eq!(field(None), MaxTokensField::MaxCompletionTokens);
        assert_eq!(
            field(Some("http://localhost:11434/v1")),
            MaxTokensField::MaxTokens
        );
    }

    #[test]
    fn preset_compat_override_refuses_an_unknown_flag() {
        let settings = |compat: &str| Settings {
            providers: [(
                "openrouter".to_string(),
                Provider {
                    compat: Some(
                        compat
                            .parse::<reagent_config::Table>()
                            .expect("a toml table"),
                    ),
                    ..Provider::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        let error = parse_with(
            &["-p", "task", "--model", "openrouter/x"],
            settings("nope = true"),
        )
        .unwrap_err();
        assert!(
            error.starts_with("config: [providers.openrouter.compat]: "),
            "{error}"
        );
        let args = run_with(
            &["-p", "task", "--model", "openrouter/x"],
            settings("max_tokens_field = \"max_completion_tokens\""),
        );
        assert_eq!(
            args.compat.map(|compat| compat.max_tokens_field),
            Some(reagent_ai::MaxTokensField::MaxCompletionTokens)
        );
    }

    #[test]
    fn a_compat_override_sets_the_reasoning_text_field() {
        let settings = Settings {
            providers: [(
                "openrouter".to_string(),
                Provider {
                    compat: Some(
                        "reasoning_field = \"reasoning\""
                            .parse::<reagent_config::Table>()
                            .expect("a toml table"),
                    ),
                    ..Provider::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task", "--model", "openrouter/x"], settings);
        assert_eq!(
            args.compat.map(|compat| compat.reasoning_field),
            Some(reagent_ai::ReasoningField::EffortObject)
        );
    }

    #[test]
    fn reasoning_off_a_config_word_sets_the_off_word() {
        let settings = Settings {
            providers: [(
                "openrouter".to_string(),
                Provider {
                    compat: Some(
                        "reasoning_off = \"disabled\""
                            .parse::<reagent_config::Table>()
                            .expect("a toml table"),
                    ),
                    ..Provider::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        let args = run_with(&["-p", "task", "--model", "openrouter/x"], settings);
        assert_eq!(
            args.compat.map(|compat| compat.reasoning_off),
            Some(Some("disabled"))
        );
    }

    #[test]
    fn plugins_flag_parses_as_a_run() {
        // The folder is the machine's, so the test reads the flag, not the path.
        let args = run_with(&["-p", "task", "--no-plugins"], Settings::default());
        assert!(args.task.is_some());
    }

    #[test]
    fn no_plugins_gives_no_plugins_folder() {
        let args = run_with(&["-p", "task", "--no-plugins"], Settings::default());
        assert_eq!(args.plugins_dir, None);
    }

    #[test]
    fn plugins_folder_that_is_missing_gives_none() {
        let tmp = tempfile::tempdir().expect("temp dir");
        assert_eq!(plugins_dir(Some(tmp.path())), None);
    }

    #[test]
    fn plugins_folder_that_exists_is_the_config_folder_plugins() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("plugins");
        make_dir(&dir);
        assert_eq!(plugins_dir(Some(tmp.path())), Some(dir));
    }

    /// Makes one folder of a test's own temp dir.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes the plugins folder of its own temp dir"
    )]
    fn make_dir(path: &Path) {
        std::fs::create_dir_all(path).expect("the plugins folder");
    }
}
