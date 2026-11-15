//! Settings read from the global and the repo `config.toml`.

pub mod auth;
pub mod glob;
pub mod mcp;
pub mod permissions;
pub mod saved;
pub mod shell;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub use toml::Table;

/// The MCP server types a run starts with.
pub use mcp::{McpOAuth, McpServer, McpTransport, load_mcp, load_mcp_for};

/// The settings a config file may hold. A key that is absent keeps the
/// built-in default.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The model text, `name/id` or a bare model id.
    pub model: Option<String>,
    /// The cap on reply tokens.
    pub max_tokens: Option<u32>,
    /// How much reasoning the provider may spend, in its own words: a level
    /// text or a token budget.
    pub reasoning: Option<String>,
    /// The loop policy names, as written in the file.
    pub policy: Option<Vec<String>>,
    /// The tool names, as written in the file.
    pub tools: Option<Vec<String>>,
    /// The MCP server names to start, with the `+name`/`-name` edits.
    pub mcp: Option<Vec<String>>,
    /// An `mcp_servers` table, refused: servers belong in `mcp.toml`.
    #[serde(default)]
    pub mcp_servers: Option<Table>,
    /// A `servers` table, refused the same way.
    #[serde(default)]
    pub servers: Option<Table>,
    /// Folder names that searches leave out.
    #[serde(default)]
    pub skip: Vec<String>,
    /// The `[codemode]` table.
    #[serde(default)]
    pub codemode: CodemodeSettings,
    /// Whether a plain file read in bash runs as a `read` call; on when unset.
    pub bash_reads: Option<bool>,
    /// How the terminal UI draws: `full` (the default) or `inline`.
    pub ui: Option<String>,
    /// The path of a TOML theme file that overrides the terminal UI's roles.
    pub theme: Option<String>,
    /// The provider tables by name. Only the global file and a profile may
    /// hold them.
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// The `[permissions]` table, as written. A repo file may hold only its
    /// `ask` and `deny` lists.
    #[serde(default)]
    pub permissions: permissions::Lists,
    /// The `[sandbox]` table. Only the global file and a profile may hold it.
    #[serde(default)]
    pub sandbox: Sandbox,
    /// The `[session]` table. Only the global file and a profile may hold it.
    #[serde(default)]
    pub session: SessionSettings,
    /// The `[gateway]` table. Only the global file and a profile may hold it.
    #[serde(default)]
    pub gateway: Gateway,
}

/// The `[sandbox]` table: how bash commands are fenced in.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    /// `off` (the default), `auto` or `on`.
    pub mode: Option<String>,
    /// Whether commands may open network connections; default false.
    pub network: Option<Network>,
    /// Folders commands may write besides the working and temp folders.
    #[serde(default)]
    pub write: Vec<String>,
}

/// The `[sandbox] network` setting: `true` or `false`, or the `host:port`
/// entries a command may reach through the run's proxy. Only allow entries
/// exist; there is no deny form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// Whether a command may open any connection.
    Open(bool),
    /// The hosts a command may reach, and nothing else.
    Hosts(Vec<Host>),
}

impl Network {
    /// The `host:port` entries, empty when the value is a bool.
    pub fn hosts(&self) -> &[Host] {
        match self {
            Network::Open(_) => &[],
            Network::Hosts(hosts) => hosts,
        }
    }
}

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Network, D::Error> {
        use serde::de::{Error, SeqAccess, Visitor};

        struct Value;

        impl<'de> Visitor<'de> for Value {
            type Value = Network;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("true, false or a list of \"host:port\" entries")
            }

            fn visit_bool<E: Error>(self, open: bool) -> Result<Network, E> {
                Ok(Network::Open(open))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Network, A::Error> {
                let mut hosts = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(text) = seq.next_element::<String>()? {
                    hosts.push(Host::parse(&text).map_err(A::Error::custom)?);
                }
                Ok(Network::Hosts(hosts))
            }
        }

        deserializer.deserialize_any(Value)
    }
}

/// One `host:port` entry of `[sandbox] network`, whose host is compared
/// without case and whose IP literals are compared as addresses. An IPv6 host
/// is written in brackets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    host: String,
    port: u16,
}

impl Host {
    /// Parses `host:port`, or names the entry in the reason it refuses it.
    pub fn parse(text: &str) -> Result<Host, String> {
        let bad = |why: &str| format!("sandbox.network entry \"{text}\" {why}");
        let (host, port) = match text.strip_prefix('[') {
            Some(bracketed) => {
                let end = bracketed.find(']').ok_or_else(|| bad("has no closing ]"))?;
                let port = bracketed[end + 1..]
                    .strip_prefix(':')
                    .ok_or_else(|| bad("needs a port after the ]"))?;
                (&bracketed[..end], port)
            }
            None => text
                .rsplit_once(':')
                .ok_or_else(|| bad("is not host:port"))?,
        };
        if host.is_empty() {
            return Err(bad("has no host"));
        }
        // An IPv6 address goes in brackets and nothing else does, so a `:`
        // never splits a host in two ways.
        let bracketed = text.starts_with('[');
        if bracketed && host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(bad(
                "has brackets around something that is not an IPv6 address",
            ));
        }
        if !bracketed && (host.contains(':') || host.contains('@')) {
            return Err(bad(
                "has a : or @ in its host; write an IPv6 address in brackets",
            ));
        }
        if host
            .chars()
            .any(|ch| ch == ' ' || ch == '/' || ch == '*' || ch.is_control())
        {
            return Err(bad("has a space, / or * in its host"));
        }
        let port: u16 = port
            .parse()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| bad("needs a port 1-65535"))?;
        Ok(Host {
            host: host.to_string(),
            port,
        })
    }

    /// The host, without the brackets of an IPv6 address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the entry names this machine, which the sandbox allows without
    /// the proxy.
    pub fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self.host == "127.0.0.1"
            || self.host == "::1"
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// Days a session is kept after its last write when `keep_days` is unset.
pub const DEFAULT_KEEP_DAYS: u64 = 30;

/// MiB all sessions may take when `max_mb` is unset.
pub const DEFAULT_MAX_MB: u64 = 1024;

/// The `[session]` table: when saved sessions are deleted.
#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionSettings {
    /// Days a session is kept after its last write; 0 keeps every age.
    pub keep_days: Option<u64>,
    /// MiB the sessions of all folders may take; 0 means no cap.
    pub max_mb: Option<u64>,
}

/// Hours a parked question waits for its answer when `gateway.deadline` is unset.
pub const DEFAULT_DEADLINE_HOURS: u64 = 72;

/// The `[gateway]` table: the questions `reagent gateway` takes, and how long
/// one may wait. Only the global file and a profile may hold it.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    /// How long a question waits: a number with `s`, `m`, `h` or `d`, or
    /// `off` for a question that never expires.
    pub deadline: Option<String>,
}

impl Gateway {
    /// How long a question waits, or `None` when it never expires.
    ///
    /// The unit is one of `s`, `m`, `h` and `d`, so `30s`, `2h` and `3d` all
    /// read, and a whole number of at least one is required.
    pub fn deadline(&self) -> Result<Option<Duration>, String> {
        let Some(text) = &self.deadline else {
            return Ok(Some(Duration::from_secs(DEFAULT_DEADLINE_HOURS * 3_600)));
        };
        if text == "off" {
            return Ok(None);
        }
        let wrong = || {
            format!(
                "gateway.deadline must be a whole number with s, m, h or d, or off, not {text:?}"
            )
        };
        let mut chars = text.chars();
        let seconds = match chars.next_back() {
            Some('s') => 1,
            Some('m') => 60,
            Some('h') => 3_600,
            Some('d') => 86_400,
            _ => return Err(wrong()),
        };
        let count: u64 = chars.as_str().parse().map_err(|_| wrong())?;
        if count == 0 {
            return Err(wrong());
        }
        Ok(Some(Duration::from_secs(count.saturating_mul(seconds))))
    }
}

/// How the terminal UI draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ui {
    /// The whole terminal, with the sidebar.
    Full,
    /// The live area under the shell's scrollback.
    Inline,
}

impl Settings {
    /// The `ui` setting, or `None` when no layer set it.
    ///
    /// An unknown value is an error that names the key, so a typo in a config
    /// file is never silently ignored.
    pub fn ui(&self) -> Result<Option<Ui>, String> {
        match self.ui.as_deref() {
            None => Ok(None),
            Some("full") => Ok(Some(Ui::Full)),
            Some("inline") => Ok(Some(Ui::Inline)),
            Some(other) => Err(format!(
                "config: ui must be full or inline, not \"{other}\""
            )),
        }
    }

    /// The path of the theme file, `None` when no layer set one and when the
    /// path is empty. The app reads the file once, at start.
    pub fn theme(&self) -> Option<&Path> {
        self.theme
            .as_deref()
            .filter(|path| !path.is_empty())
            .map(Path::new)
    }
}

impl SessionSettings {
    /// Days a session is kept, [`DEFAULT_KEEP_DAYS`] when unset; 0 keeps every age.
    pub fn keep_days(&self) -> u64 {
        self.keep_days.unwrap_or(DEFAULT_KEEP_DAYS)
    }

    /// MiB all sessions may take, [`DEFAULT_MAX_MB`] when unset; 0 means no cap.
    pub fn max_mb(&self) -> u64 {
        self.max_mb.unwrap_or(DEFAULT_MAX_MB)
    }
}

/// The `[codemode]` table: the limits of one `codemode` script.
#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodemodeSettings {
    /// The longest one script may run, in seconds; 600 when unset.
    pub wall_time: Option<u64>,
}

/// A secret string. Its `Debug` output hides the text, so settings printed
/// in a log, a panic or an error never show it. It has no `Serialize`, so a
/// struct that holds one cannot print it either; a key reaches a file only
/// through the field-by-field write in `auth`.
#[derive(Deserialize, Clone, PartialEq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The secret text. Use it only to build a request.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Secret {
    fn from(text: &str) -> Self {
        Secret(text.to_string())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// A `[providers.<name>]` table.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// The wire name, as for `--provider`.
    pub wire: Option<String>,
    /// The preset this table takes its defaults from. A table without one is
    /// a preset table when a preset has its name.
    pub preset: Option<String>,
    /// The models.dev provider id its model list is filled from, over the
    /// preset's own id.
    pub models_dev: Option<String>,
    /// The saved login this provider signs in with. Defaults to its own name.
    pub login: Option<String>,
    /// The endpoint URL.
    pub base_url: Option<String>,
    /// The name of the environment variable holding the API key.
    pub api_key_env: Option<String>,
    /// The API key itself. The global file must then be readable by you only.
    pub api_key: Option<Secret>,
    /// Extra request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// How the endpoint differs from the plain wire, as written. The caller
    /// checks the flags.
    pub compat: Option<Table>,
    /// How much reasoning this provider's replies may spend.
    pub reasoning: Option<String>,
    /// The per-model tables by model id.
    #[serde(default)]
    pub models: BTreeMap<String, Model>,
}

/// A `[providers.<name>.models."<id>"]` table.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// The cap on reply tokens for this model.
    pub max_tokens: Option<u32>,
    /// How much reasoning this model's replies may spend.
    pub reasoning: Option<String>,
    /// The size of the context window in tokens.
    pub context_window: Option<u64>,
    /// The loop policy names for this model.
    pub policy: Option<Vec<String>>,
    /// What the model costs, over any price a model list gives.
    pub price: Option<ModelPrice>,
}

/// A `[providers.<name>.models."<id>".price]` table: USD per 1M tokens.
#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// USD per 1M input tokens.
    #[serde(deserialize_with = "usd")]
    pub input: f64,
    /// USD per 1M output tokens.
    #[serde(deserialize_with = "usd")]
    pub output: f64,
    /// USD per 1M input tokens read from the prompt cache.
    #[serde(deserialize_with = "usd")]
    pub cached: f64,
    /// USD per 1M input tokens written to the prompt cache; 0 when absent.
    #[serde(default, deserialize_with = "usd_option")]
    pub cache_write: Option<f64>,
}

/// Reads one USD price. A negative or non-finite amount is a config error.
fn usd<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let amount = f64::deserialize(deserializer)?;
    if !amount.is_finite() || amount < 0.0 {
        return Err(serde::de::Error::custom(
            "a price must be a finite amount of at least 0",
        ));
    }
    Ok(amount)
}

/// Reads an optional USD price, checked like [`usd`].
fn usd_option<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    usd(deserializer).map(Some)
}

/// The settings of one config file with the file's path.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    /// The file the settings came from.
    pub path: PathBuf,
    /// The settings read from it.
    pub settings: Settings,
}

/// The config files of one run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layers {
    /// The global file.
    pub global: Option<Layer>,
    /// The profile file, read as the global file is.
    pub profile: Option<Layer>,
    /// The repo `.reagent/config.toml`.
    pub repo: Option<Layer>,
    /// The allow rules saved for this project; see [`saved`].
    pub saved: Option<Layer>,
}

impl Layers {
    /// The layers in precedence order: the global file, the profile, the repo
    /// file, then the saved rules of the project.
    pub fn ordered(&self) -> Vec<Layer> {
        [&self.global, &self.profile, &self.repo, &self.saved]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

/// The nearest folder at or above `cwd` that holds `.git`, a folder or a file.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn global_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg.filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|value| !value.is_empty())?).join(".config"),
    };
    Some(base.join("reagent"))
}

/// The global config folder: under `XDG_CONFIG_HOME` when it is set, else
/// under `$HOME/.config`.
pub fn global_dir() -> Option<PathBuf> {
    global_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// The global config file path.
pub fn global_path() -> Option<PathBuf> {
    global_dir().map(|dir| dir.join("config.toml"))
}

/// The text of a file and whether group or others may read it.
pub(crate) struct Text {
    pub(crate) text: String,
    pub(crate) shared: bool,
}

#[expect(
    clippy::disallowed_types,
    reason = "the config is read before the runtime starts"
)]
fn read(path: &Path) -> Result<Option<Text>, String> {
    use std::io::Read;

    let fail = |error: std::io::Error| format!("{}: {error}", path.display());
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fail(error)),
    };
    #[cfg(unix)]
    let shared = {
        use std::os::unix::fs::PermissionsExt;
        file.metadata().map_err(fail)?.permissions().mode() & 0o077 != 0
    };
    #[cfg(not(unix))]
    let shared = false;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(fail)?;
    Ok(Some(Text { text, shared }))
}

/// The parse error with its place and message but not the source line, which
/// could hold an api_key.
pub(crate) fn parse_error(path: &Path, text: &str, error: &toml::de::Error) -> String {
    // An "invalid type" or "invalid value" message quotes the value it got.
    let message = match error.message().split_once(", expected ") {
        Some((head, expected)) if head.starts_with("invalid ") => {
            format!("invalid value, expected {expected}")
        }
        _ => error.message().to_string(),
    };
    let Some(span) = error.span() else {
        return format!("{}: {message}", path.display());
    };
    let before = &text[..span.start.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.chars().rev().take_while(|&c| c != '\n').count() + 1;
    format!("{}:{line}:{column}: {message}", path.display())
}

/// The text of a config file and whether others may read it, or `None` when
/// the file is absent.
pub(crate) fn read_text(path: &Path) -> Result<Option<Text>, String> {
    read(path)
}

fn load_file(path: Option<&Path>, repo: bool) -> Result<Option<Layer>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(Text { text, shared }) = read(path)? else {
        return Ok(None);
    };
    let settings: Settings =
        toml::from_str(&text).map_err(|error| parse_error(path, &text, &error))?;
    if settings.mcp_servers.is_some() || settings.servers.is_some() {
        return Err(format!(
            "servers belong in mcp.toml, not in {}",
            path.display()
        ));
    }
    if !repo && shared && settings.providers.values().any(|p| p.api_key.is_some()) {
        return Err(format!(
            "{0}: holds an api_key, so only you may read it; run chmod 600 {0}",
            path.display()
        ));
    }
    if repo && !settings.providers.is_empty() {
        return Err(format!(
            "{}: providers may be set only in the global config",
            path.display()
        ));
    }
    if repo && settings.sandbox != Sandbox::default() {
        return Err(format!(
            "{}: sandbox may be set only in the global config",
            path.display()
        ));
    }
    if repo && settings.session != SessionSettings::default() {
        return Err(format!(
            "{}: session may be set only in the global config",
            path.display()
        ));
    }
    if repo && settings.gateway != Gateway::default() {
        return Err(format!(
            "{}: gateway may be set only in the global config",
            path.display()
        ));
    }
    if let Err(reason) = settings.gateway.deadline() {
        return Err(format!("{}: {reason}", path.display()));
    }
    if repo && (!settings.permissions.allow.is_empty() || settings.permissions.default.is_some()) {
        return Err(format!(
            "{}: permissions.allow and permissions.default may be set only in the global config",
            path.display()
        ));
    }
    Ok(Some(Layer {
        path: path.to_path_buf(),
        settings,
    }))
}

/// Loads the global file, the file of the profile `profile` when the run
/// names one, the repo file for `cwd`, and the rules saved for its project.
///
/// There is no saved layer when the state folder has no path (no `HOME` and
/// no `XDG_STATE_HOME`), which is not an error.
pub fn load_for(cwd: &Path, profile: Option<&str>) -> Result<Layers, String> {
    load_for_state(cwd, profile, auth::dir().ok().as_deref())
}

/// [`load_for`] with the state folder named, so a test needs no environment.
fn load_for_state(
    cwd: &Path,
    profile: Option<&str>,
    state: Option<&Path>,
) -> Result<Layers, String> {
    let repo = repo_root(cwd).map(|root| root.join(".reagent").join("config.toml"));
    let saved = state
        .map(|state| saved::load(&saved::path(state, cwd)))
        .transpose()?
        .flatten();
    Ok(Layers {
        global: load_file(global_path().as_deref(), false)?,
        profile: profile.map(load_profile).transpose()?,
        repo: load_file(repo.as_deref(), true)?,
        saved,
    })
}

/// Whether `name` can name a provider, a login or a profile: letters,
/// digits, `-` and `_`, not starting with `-`. Such a name is also a safe
/// file name, and a leading `-` would read as a flag on the command line.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The profile file `name` under the config folder `dir`, or the error that
/// names the folder it is not in.
fn load_profile_in(dir: &Path, name: &str) -> Result<Layer, String> {
    if !valid_name(name) {
        return Err(format!(
            "invalid profile {name}: use letters, digits, - and _ only, not starting with -"
        ));
    }
    let folder = dir.join("profiles");
    let path = folder.join(format!("{name}.toml"));
    load_file(Some(&path), false)?
        .ok_or_else(|| format!("no profile {name} in {}", folder.display()))
}

/// The profile file `name` under the global folder.
fn load_profile(name: &str) -> Result<Layer, String> {
    match global_dir() {
        Some(dir) => load_profile_in(&dir, name),
        None => Err(format!("no profile {name}: set XDG_CONFIG_HOME or HOME")),
    }
}

/// The error a list that mixes plain names and `+name`/`-name` edits gets.
pub const LIST_MIXED: &str = "a list takes plain names or +name/-name edits, not both";

/// Whether `entry` adds or removes a name, written with a `+` or `-` prefix.
pub fn is_edit(entry: &str) -> bool {
    entry.starts_with(['+', '-'])
}

/// The name `entry` adds or removes: the entry without its `+` or `-`.
pub fn list_name(entry: &str) -> &str {
    entry.strip_prefix(['+', '-']).unwrap_or(entry)
}

/// Checks that `layer` is plain names or edits, never both in one list.
pub fn check_list(layer: &[String]) -> Result<(), String> {
    if layer.iter().any(|entry| is_edit(entry)) && !layer.iter().all(|entry| is_edit(entry)) {
        return Err(LIST_MIXED.to_string());
    }
    Ok(())
}

/// Folds one layer's list into `merged`, or errors when it mixes the two forms.
///
/// Plain names replace `merged`, or add to it when `replace` is false; the
/// edits of a later layer are appended in order.
pub fn fold_list(merged: &mut Vec<String>, layer: &[String], replace: bool) -> Result<(), String> {
    check_list(layer)?;
    fold(merged, layer, replace);
    Ok(())
}

/// [`fold_list`] for a layer that [`check_list`] already passed.
fn fold(merged: &mut Vec<String>, layer: &[String], replace: bool) {
    if layer.iter().any(|entry| is_edit(entry)) {
        merged.extend(layer.iter().cloned());
        return;
    }
    if replace {
        merged.clear();
        merged.extend(layer.iter().cloned());
        if layer.is_empty() {
            merged.push(EMPTY.to_string());
        }
        return;
    }
    // After an earlier layer's edits, a plain name must land after them, or a
    // `-name` of that earlier layer would remove it again.
    if merged.iter().any(|entry| is_edit(entry)) {
        merged.extend(layer.iter().map(|name| format!("+{name}")));
        return;
    }
    for name in layer {
        if !merged.iter().any(|kept| kept == name) {
            merged.push(name.clone());
        }
    }
}

/// The plain part of a list that a layer replaced with an empty list, so that
/// [`resolve_list`] can tell it from a list that no layer replaced.
const EMPTY: &str = "";

/// Turns a folded list into the names it selects, sorted and de-duplicated.
///
/// The plain part is the base, or `default` when no layer replaced it; then
/// the edits apply in order.
pub fn resolve_list(merged: &[String], default: &[&str]) -> Vec<String> {
    let plain = merged.iter().take_while(|entry| !is_edit(entry)).count();
    let mut names: Vec<String> = if plain == 0 {
        default.iter().map(|name| (*name).to_string()).collect()
    } else {
        merged[..plain]
            .iter()
            .filter(|name| name.as_str() != EMPTY)
            .cloned()
            .collect()
    };
    for entry in &merged[plain..] {
        match entry.strip_prefix('-') {
            Some(name) => names.retain(|kept| kept != name),
            None => names.push(list_name(entry).to_string()),
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Merges the layers of a run, a later layer over an earlier one for `model`,
/// `max_tokens`, `reasoning`, `tools`, `policy`, `mcp` and the `[codemode]`
/// table; `skip` lists are appended and de-duplicated in order. Providers and
/// the sandbox come from the global file, and a profile's tables merge over
/// them field by field.
pub fn merge_all(layers: &Layers) -> Settings {
    let mut merged = Settings::default();
    for (layer, profile) in [
        (&layers.global, false),
        (&layers.profile, true),
        (&layers.repo, false),
    ] {
        if let Some(layer) = layer {
            apply(&mut merged, &layer.settings, profile);
        }
    }
    merged
}

/// `base` takes `value` when it is set, which is how a later layer wins one
/// field.
fn over<T: Clone>(base: &mut Option<T>, value: &Option<T>) {
    if value.is_some() {
        *base = value.clone();
    }
}

/// Merges one layer's settings over `merged`. A profile layer's provider
/// tables merge over the ones there; any other layer's providers are kept
/// only when no layer before it held any.
fn apply(merged: &mut Settings, settings: &Settings, profile: bool) {
    over(&mut merged.model, &settings.model);
    over(&mut merged.max_tokens, &settings.max_tokens);
    over(&mut merged.reasoning, &settings.reasoning);
    over(&mut merged.codemode.wall_time, &settings.codemode.wall_time);
    if let Some(list) = &settings.policy {
        fold(merged.policy.get_or_insert_with(Vec::new), list, true);
    }
    if let Some(list) = &settings.tools {
        fold(merged.tools.get_or_insert_with(Vec::new), list, true);
    }
    if let Some(list) = &settings.mcp {
        fold(merged.mcp.get_or_insert_with(Vec::new), list, true);
    }
    over(&mut merged.bash_reads, &settings.bash_reads);
    over(&mut merged.ui, &settings.ui);
    over(&mut merged.theme, &settings.theme);
    fold(&mut merged.skip, &settings.skip, false);
    if profile {
        for (name, table) in &settings.providers {
            match merged.providers.get_mut(name) {
                Some(base) => merge_provider(base, table),
                None => {
                    merged.providers.insert(name.clone(), table.clone());
                }
            }
        }
    } else if merged.providers.is_empty() {
        merged.providers = settings.providers.clone();
    }
    if profile {
        over(&mut merged.gateway.deadline, &settings.gateway.deadline);
    } else if merged.gateway == Gateway::default() {
        merged.gateway = settings.gateway.clone();
    }
    if profile {
        over(&mut merged.sandbox.mode, &settings.sandbox.mode);
        over(&mut merged.sandbox.network, &settings.sandbox.network);
        if !settings.sandbox.write.is_empty() {
            merged.sandbox.write = settings.sandbox.write.clone();
        }
    } else if merged.sandbox == Sandbox::default() {
        merged.sandbox = settings.sandbox.clone();
    }
    over(&mut merged.session.keep_days, &settings.session.keep_days);
    over(&mut merged.session.max_mb, &settings.session.max_mb);
}

/// Merges a profile's `[providers.<name>]` table over the global one: each
/// field the profile sets replaces the global field, and the `headers` and
/// `models` maps merge key by key with the profile winning.
fn merge_provider(base: &mut Provider, profile: &Provider) {
    over(&mut base.wire, &profile.wire);
    over(&mut base.preset, &profile.preset);
    over(&mut base.models_dev, &profile.models_dev);
    over(&mut base.login, &profile.login);
    over(&mut base.base_url, &profile.base_url);
    over(&mut base.api_key_env, &profile.api_key_env);
    over(&mut base.api_key, &profile.api_key);
    over(&mut base.compat, &profile.compat);
    over(&mut base.reasoning, &profile.reasoning);
    base.headers.extend(profile.headers.clone());
    for (id, model) in &profile.models {
        match base.models.get_mut(id) {
            Some(base) => {
                over(&mut base.max_tokens, &model.max_tokens);
                over(&mut base.reasoning, &model.reasoning);
                over(&mut base.context_window, &model.context_window);
                over(&mut base.policy, &model.policy);
                over(&mut base.price, &model.price);
            }
            None => {
                base.models.insert(id.clone(), model.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("makes the folder");
        std::fs::write(&path, text).expect("writes the file");
        path
    }

    fn load(global: Option<&Path>, repo: Option<&Path>) -> Result<Vec<Layer>, String> {
        Ok([load_file(global, false)?, load_file(repo, true)?]
            .into_iter()
            .flatten()
            .collect())
    }

    fn settings(global: Option<&Path>, repo: Option<&Path>) -> Settings {
        merge_all(&Layers {
            global: load_file(global, false).expect("loads"),
            profile: None,
            repo: load_file(repo, true).expect("loads"),
            saved: None,
        })
    }

    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn folder(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).expect("makes the folder");
        path
    }

    #[test]
    fn saved_layer_comes_last_and_allows_its_rule() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = dir.path().join("state");
        let work = folder(dir.path(), "work");
        let file = saved::path(&state, &work);
        assert!(saved::add(&file, "Bash(cargo test:*)").expect("adds"));
        let layers = load_for_state(&work, None, Some(&state)).expect("loads");
        assert_eq!(
            layers.saved.as_ref().map(|layer| layer.path.clone()),
            Some(file.clone())
        );
        let ordered = layers.ordered();
        assert_eq!(ordered.last().map(|layer| &layer.path), Some(&file));
        let layer = layers.saved.expect("a saved layer");
        let rules =
            permissions::Rules::from_layers(&[layer], &work, Some(dir.path())).expect("rules");
        let decision = rules.decide(&permissions::Call {
            tool: "bash",
            main_arg: Some("command"),
            value: Some("cargo test -p b"),
            reads: &[],
            writes: &[],
        });
        assert_eq!(decision.kind, permissions::Kind::Allow);
        assert_eq!(
            decision.reason,
            permissions::Reason::Rule {
                rule: "Bash(cargo test:*)".into(),
                file: file.display().to_string().into(),
            }
        );
    }

    #[test]
    fn saved_layer_is_absent_without_a_state_folder() {
        let dir = tempfile::tempdir().expect("temp dir");
        let work = folder(dir.path(), "work");
        let layers = load_for_state(&work, None, None).expect("loads");
        assert!(layers.saved.is_none());
        assert!(layers.ordered().iter().all(|layer| layer.path != work));
    }

    #[test]
    fn profile_layer_sandbox_merges_field_by_field() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[sandbox]\nmode = \"auto\"\nwrite = [\"~/.cache\"]\n",
        );
        let profile = write(dir.path(), "p.toml", "[sandbox]\nmode = \"on\"\n");
        let merged = merge_all(&Layers {
            global: load_file(Some(&global), false).expect("loads"),
            profile: load_file(Some(&profile), false).expect("loads"),
            repo: None,
            saved: None,
        });
        assert_eq!(merged.sandbox.mode.as_deref(), Some("on"));
        assert_eq!(merged.sandbox.write, vec!["~/.cache"]);
    }

    #[test]
    fn gateway_deadline_of_the_global_file_stays_under_a_profile_without_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "g.toml", "[gateway]\ndeadline = \"off\"\n");
        let profile = write(dir.path(), "p.toml", "[sandbox]\nmode = \"on\"\n");
        let merged = merge_all(&Layers {
            global: load_file(Some(&global), false).expect("loads"),
            profile: load_file(Some(&profile), false).expect("loads"),
            repo: None,
            saved: None,
        });
        assert_eq!(merged.gateway.deadline(), Ok(None));
        let over = write(dir.path(), "o.toml", "[gateway]\ndeadline = \"2h\"\n");
        let merged = merge_all(&Layers {
            global: load_file(Some(&global), false).expect("loads"),
            profile: load_file(Some(&over), false).expect("loads"),
            repo: None,
            saved: None,
        });
        assert_eq!(
            merged.gateway.deadline(),
            Ok(Some(Duration::from_secs(7_200)))
        );
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmods");
    }

    #[cfg(unix)]
    #[test]
    fn an_api_key_loads_from_a_private_global_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        chmod(&global, 0o600);
        let merged = settings(Some(&global), None);
        let key = merged.providers["fi"].api_key.as_ref().expect("has a key");
        assert_eq!(key.expose(), "sk-test-123");
    }

    #[cfg(unix)]
    #[test]
    fn an_api_key_in_a_shared_global_file_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        chmod(&global, 0o644);
        let error = load(Some(&global), None).unwrap_err();
        let path = global.display();
        assert_eq!(
            error,
            format!("{path}: holds an api_key, so only you may read it; run chmod 600 {path}")
        );
        assert!(!error.contains("sk-test-123"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_global_file_without_an_api_key_loads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key_env = \"FI_KEY\"\n",
        );
        chmod(&global, 0o644);
        assert!(load(Some(&global), None).is_ok());
    }

    #[test]
    fn a_repo_file_may_not_hold_an_api_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = write(
            dir.path(),
            "repo.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        let error = load(None, Some(&repo)).unwrap_err();
        assert!(error.contains("providers"), "{error}");
        assert!(!error.contains("sk-test-123"), "{error}");
    }

    #[test]
    fn the_debug_text_of_an_api_key_is_hidden() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        #[cfg(unix)]
        chmod(&global, 0o600);
        let merged = settings(Some(&global), None);
        let text = format!("{merged:?}");
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("sk-test-123"), "{text}");
    }

    #[test]
    fn a_parse_error_never_shows_the_api_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        for text in [
            "model = \"m\"\n[providers.fi]\napi_key = sk-test-SECRET\n",
            "[providers.fi]\napi_key = \"sk-test-SECRET\n",
            "[providers.fi]\napi_key = 12345678\n",
        ] {
            let global = write(dir.path(), "g.toml", text);
            let error = load(Some(&global), None).unwrap_err();
            assert!(!error.contains("SECRET"), "{error}");
            assert!(!error.contains("12345678"), "{error}");
            assert!(error.contains("g.toml:"), "{error}");
            assert!(error.chars().any(|c| c.is_ascii_digit()), "{error}");
        }
    }

    #[test]
    fn a_parse_error_names_the_line_and_the_secret_free_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "g.toml", "model = \"m\"\nmodle = \"x\"\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("g.toml:2:1"), "{error}");
        assert!(error.contains("modle"), "{error}");
    }

    #[test]
    fn a_global_file_sets_the_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "model = \"glm\"\n");
        assert_eq!(settings(Some(&global), None).model.as_deref(), Some("glm"));
    }

    #[test]
    fn reasoning_text_merges_like_max_tokens() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "reasoning = \"high\"\n");
        assert_eq!(
            settings(Some(&global), None).reasoning.as_deref(),
            Some("high")
        );
        // The repo file may set it, as it may set max_tokens.
        let repo = write(dir.path(), "repo.toml", "reasoning = \"4096\"\n");
        let merged = settings(Some(&global), Some(&repo));
        assert_eq!(merged.reasoning.as_deref(), Some("4096"));
    }

    #[test]
    fn reasoning_text_is_read_from_a_provider_and_its_model_tables() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            concat!(
                "[providers.fi]\n",
                "reasoning = \"low\"\n",
                "[providers.fi.models.\"glm\"]\n",
                "reasoning = \"none\"\n",
            ),
        );
        let merged = settings(Some(&global), None);
        let provider = &merged.providers["fi"];
        assert_eq!(provider.reasoning.as_deref(), Some("low"));
        assert_eq!(provider.models["glm"].reasoning.as_deref(), Some("none"));
    }

    #[test]
    fn the_repo_file_beats_the_global_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            "model = \"glm\"\nmax_tokens = 100\n[providers.fi]\nwire = \"openai-chat\"\n",
        );
        let repo = write(dir.path(), "repo.toml", "model = \"kimi\"\n");
        let merged = settings(Some(&global), Some(&repo));
        assert_eq!(merged.model.as_deref(), Some("kimi"));
        assert_eq!(merged.max_tokens, Some(100));
        assert_eq!(merged.providers["fi"].wire.as_deref(), Some("openai-chat"));
    }

    fn named(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn list_edit_fold_and_resolve() {
        let mut merged = Vec::new();
        fold_list(&mut merged, &named(&["read", "grep"]), true).expect("folds");
        assert_eq!(merged, ["read", "grep"]);
        fold_list(&mut merged, &named(&["+write", "-read"]), true).expect("folds");
        assert_eq!(merged, ["read", "grep", "+write", "-read"]);
        assert_eq!(resolve_list(&merged, &["bash"]), ["grep", "write"]);
        let error = fold_list(&mut merged, &named(&["edit", "+bash"]), true).unwrap_err();
        assert!(error.contains("not both"), "{error}");
        // Skip's plain names add to the list instead of replacing it.
        let mut added = Vec::new();
        fold_list(&mut added, &named(&["a", "b"]), false).expect("folds");
        fold_list(&mut added, &named(&["b", "c"]), false).expect("folds");
        assert_eq!(added, ["a", "b", "c"]);
        // A later plain name comes after an earlier removal and wins.
        let mut back = Vec::new();
        fold_list(&mut back, &named(&["vendor"]), false).expect("folds");
        fold_list(&mut back, &named(&["-vendor"]), false).expect("folds");
        fold_list(&mut back, &named(&["vendor"]), false).expect("folds");
        assert_eq!(resolve_list(&back, &[]), ["vendor"]);
        // Without a plain part the base is the default one.
        assert_eq!(resolve_list(&named(&["-x"]), &["x", "y"]), ["y"]);
        assert_eq!(resolve_list(&[], &["x"]), ["x"]);
        assert_eq!(resolve_list(&named(&["b", "a"]), &["x"]), ["a", "b"]);
        // An empty plain layer selects nothing, and later edits build on that.
        let mut cleared = Vec::new();
        fold_list(&mut cleared, &[], true).expect("folds");
        assert!(resolve_list(&cleared, &["x"]).is_empty());
        fold_list(&mut cleared, &named(&["+y"]), true).expect("folds");
        assert_eq!(resolve_list(&cleared, &["x"]), ["y"]);
        assert_eq!(list_name("+write"), "write");
        assert_eq!(list_name("-write"), "write");
        assert_eq!(list_name("write"), "write");
        assert!(is_edit("+write") && is_edit("-write") && !is_edit("write"));
    }

    #[test]
    fn codemode_table_sets_the_wall_time() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "g.toml", "[codemode]\nwall_time = 30\n");
        assert_eq!(settings(Some(&global), None).codemode.wall_time, Some(30));
        let repo = write(dir.path(), "r.toml", "[codemode]\nwall_time = 20\n");
        let merged = settings(Some(&global), Some(&repo));
        assert_eq!(merged.codemode.wall_time, Some(20));
        assert_eq!(settings(None, None).codemode.wall_time, None);
    }

    #[test]
    fn codemode_table_unknown_key_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "g.toml", "[codemode]\nnope = 1\n");
        let error = load_file(Some(&global), false).unwrap_err();
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn skip_lists_add_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "skip = [\"a\", \"b\", \"a\"]\n");
        let repo = write(dir.path(), "repo.toml", "skip = [\"b\", \"c\"]\n");
        assert_eq!(
            settings(Some(&global), Some(&repo)).skip,
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn a_repo_file_may_not_define_providers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = write(
            dir.path(),
            "repo.toml",
            "[providers.evil]\nbase_url = \"http://evil\"\n",
        );
        let error = load(None, Some(&repo)).unwrap_err();
        assert!(error.contains("repo.toml"), "{error}");
        assert!(error.contains("providers"), "{error}");
    }

    #[test]
    fn a_repo_permission_file_may_add_only_ask_and_deny() {
        let dir = tempfile::tempdir().expect("temp dir");
        for text in [
            "[permissions]\nallow = [\"Bash\"]\n",
            "[permissions]\ndefault = \"allow\"\n",
        ] {
            let repo = write(dir.path(), "repo.toml", text);
            let error = load(None, Some(&repo)).unwrap_err();
            assert!(error.contains("repo.toml"), "{error}");
            assert!(error.contains("permissions"), "{error}");
        }
        let repo = write(
            dir.path(),
            "repo.toml",
            "[permissions]\ndeny = [\"Bash(git push:*)\"]\nask = [\"Bash(rm:*)\"]\n",
        );
        let layers = load(None, Some(&repo)).expect("deny and ask are fine");
        assert_eq!(
            layers[0].settings.permissions.deny,
            vec!["Bash(git push:*)"]
        );
    }

    #[test]
    fn session_table_defaults_and_merges() {
        let dir = tempfile::tempdir().expect("temp dir");
        let none = settings(None, None);
        assert_eq!(none.session.keep_days(), DEFAULT_KEEP_DAYS);
        assert_eq!(none.session.max_mb(), DEFAULT_MAX_MB);
        let global = write(
            dir.path(),
            "g.toml",
            "[session]\nkeep_days = 7\nmax_mb = 512\n",
        );
        let profile = write(dir.path(), "p.toml", "[session]\nmax_mb = 0\n");
        let merged = merge_all(&Layers {
            global: load_file(Some(&global), false).expect("loads"),
            profile: load_file(Some(&profile), false).expect("loads"),
            repo: None,
            saved: None,
        });
        assert_eq!(merged.session.keep_days(), 7);
        assert_eq!(merged.session.max_mb(), 0, "0 turns the cap off");
    }

    #[test]
    fn session_table_is_refused_in_a_repo_file_and_checks_its_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = write(dir.path(), "repo.toml", "[session]\nkeep_days = 1\n");
        let error = load(None, Some(&repo)).unwrap_err();
        assert!(
            error.contains("repo.toml") && error.contains("session"),
            "{error}"
        );
        let typo = write(dir.path(), "t.toml", "[session]\nkeep_day = 1\n");
        assert!(load(Some(&typo), None).unwrap_err().contains("keep_day"));
    }

    #[test]
    fn sandbox_settings_come_only_from_the_global_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[sandbox]\nmode = \"auto\"\nnetwork = true\nwrite = [\"~/.cache\"]\n",
        );
        let merged = settings(Some(&global), None);
        assert_eq!(merged.sandbox.mode.as_deref(), Some("auto"));
        assert_eq!(merged.sandbox.network, Some(Network::Open(true)));
        assert_eq!(merged.sandbox.write, vec!["~/.cache"]);
        let repo = write(dir.path(), "repo.toml", "[sandbox]\nmode = \"off\"\n");
        let error = load(Some(&global), Some(&repo)).unwrap_err();
        assert!(
            error.contains("repo.toml") && error.contains("sandbox"),
            "{error}"
        );
        let typo = write(dir.path(), "t.toml", "[sandbox]\nmod = \"on\"\n");
        assert!(load(Some(&typo), None).unwrap_err().contains("mod"));
    }

    #[test]
    fn gateway_deadline_parses_its_spans() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (text, seconds) in [("30s", 30), ("72h", 72 * 3_600), ("2d", 2 * 86_400)] {
            let file = write(
                dir.path(),
                "config.toml",
                &format!("[gateway]\ndeadline = \"{text}\"\n"),
            );
            assert_eq!(
                settings(Some(&file), None).gateway.deadline(),
                Ok(Some(Duration::from_secs(seconds))),
                "{text}"
            );
        }
    }

    #[test]
    fn gateway_deadline_off_never_expires() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = write(dir.path(), "config.toml", "[gateway]\ndeadline = \"off\"\n");
        assert_eq!(settings(Some(&file), None).gateway.deadline(), Ok(None));
    }

    #[test]
    fn gateway_deadline_is_72_hours_when_unset() {
        assert_eq!(
            settings(None, None).gateway.deadline(),
            Ok(Some(Duration::from_secs(72 * 3_600)))
        );
    }

    #[test]
    fn gateway_deadline_a_wrong_value_names_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        for text in ["3w", "72", "", "2x"] {
            let file = write(
                dir.path(),
                "config.toml",
                &format!("[gateway]\ndeadline = \"{text}\"\n"),
            );
            let error = load(Some(&file), None).unwrap_err();
            assert!(
                error.contains(&format!("{text:?}")) && error.contains("gateway.deadline"),
                "{error}"
            );
        }
    }

    #[test]
    fn gateway_table_is_refused_in_a_repo_file_and_checks_its_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = write(dir.path(), "repo.toml", "[gateway]\ndeadline = \"2h\"\n");
        let error = load(None, Some(&repo)).unwrap_err();
        assert!(
            error.contains("repo.toml") && error.contains("gateway"),
            "{error}"
        );
        let typo = write(dir.path(), "t.toml", "[gateway]\ndeadlin = \"2h\"\n");
        assert!(load(Some(&typo), None).unwrap_err().contains("deadlin"));
    }

    #[test]
    fn host_list_parses_into_the_network_setting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            concat!(
                "[sandbox]\n",
                "network = [\"devbox.example.net:22\", \"GitHub.com:443\",",
                " \"127.0.0.1:8080\", \"[::1]:9000\"]\n",
            ),
        );
        let merged = settings(Some(&global), None);
        let Some(Network::Hosts(hosts)) = merged.sandbox.network else {
            panic!("not a host list: {:?}", merged.sandbox.network);
        };
        assert_eq!(
            hosts.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "devbox.example.net:22",
                "GitHub.com:443",
                "127.0.0.1:8080",
                "[::1]:9000",
            ]
        );
        assert_eq!(hosts[0].host(), "devbox.example.net");
        assert_eq!(hosts[0].port(), 22);
        assert_eq!(hosts[3].host(), "::1");
        assert!(hosts[2].is_loopback() && hosts[3].is_loopback());
        assert!(!hosts[0].is_loopback());
        // An empty list is no network, and no error.
        assert_eq!(Host::parse("h:1").map(|host| host.port()), Ok(1));
        assert_eq!(Network::Hosts(Vec::new()).hosts().len(), 0);
    }

    #[test]
    fn host_list_entry_forms_are_refused_with_their_text() {
        for text in [
            "",
            "nohost",
            ":443",
            "github.com:",
            "github.com:0",
            "github.com:70000",
            "github.com:port",
            "git hub.com:443",
            "github.com/x:443",
            "*.github.com:443",
            "[::1]",
            "[::1:8080",
            "[]:8080",
        ] {
            let refused = Host::parse(text).unwrap_err();
            assert!(refused.contains(&format!("\"{text}\"")), "{refused}");
            assert!(refused.contains("sandbox.network entry"), "{refused}");
            let dir = tempfile::tempdir().expect("temp dir");
            let path = write(
                dir.path(),
                "g.toml",
                &format!("[sandbox]\nnetwork = [\"{text}\"]\n"),
            );
            let error = load(Some(&path), None).unwrap_err();
            assert!(error.contains(&format!("\"{text}\"")), "{text}: {error}");
        }
    }

    #[test]
    fn host_list_a_host_with_a_colon_or_an_at_is_refused() {
        for entry in [
            "::1:80",
            "a:b:80",
            "user@host:22",
            "[example.com]:80",
            "[]:80",
        ] {
            assert!(Host::parse(entry).is_err(), "{entry} parses");
        }
        assert_eq!(Host::parse("[::1]:80").expect("an entry").host(), "::1");
    }

    #[test]
    fn host_list_a_bool_still_parses() {
        for (text, wanted) in [("true", true), ("false", false)] {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = write(
                dir.path(),
                "g.toml",
                &format!("[sandbox]\nnetwork = {text}\n"),
            );
            let merged = settings(Some(&path), None);
            assert_eq!(merged.sandbox.network, Some(Network::Open(wanted)));
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write(dir.path(), "g.toml", "[sandbox]\nnetwork = [\"h:1\", 3]\n");
        assert!(load(Some(&path), None).is_err());
    }

    #[test]
    fn host_list_a_profile_replaces_the_network_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[sandbox]\nnetwork = [\"a.example:1\"]\n",
        );
        let profile = write(dir.path(), "p.toml", "[sandbox]\nnetwork = true\n");
        let merged = merge_all(&Layers {
            global: load_file(Some(&global), false).expect("loads"),
            profile: load_file(Some(&profile), false).expect("loads"),
            repo: None,
            saved: None,
        });
        assert_eq!(merged.sandbox.network, Some(Network::Open(true)));
    }

    #[test]
    fn a_permission_rule_typo_is_an_unknown_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "g.toml", "[permissions]\ndenny = []\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("denny"), "{error}");
    }

    #[test]
    fn providers_and_models_parse() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            concat!(
                "[providers.fi]\n",
                "wire = \"openai-chat\"\n",
                "base_url = \"https://fi.example/v1\"\n",
                "api_key_env = \"FI_KEY\"\n",
                "[providers.fi.headers]\n",
                "x-team = \"a\"\n",
                "[providers.fi.models.\"glm-5.3-flash\"]\n",
                "max_tokens = 16000\n",
                "context_window = 128000\n",
                "policy = [\"budget\"]\n",
            ),
        );
        let merged = settings(Some(&global), None);
        let provider = &merged.providers["fi"];
        assert_eq!(provider.wire.as_deref(), Some("openai-chat"));
        assert_eq!(provider.base_url.as_deref(), Some("https://fi.example/v1"));
        assert_eq!(provider.api_key_env.as_deref(), Some("FI_KEY"));
        assert_eq!(provider.headers["x-team"], "a");
        let model = &provider.models["glm-5.3-flash"];
        assert_eq!(model.max_tokens, Some(16000));
        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.policy, Some(vec!["budget".to_string()]));
    }

    #[test]
    fn price_rule_a_config_price_is_usd_per_million_tokens() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            concat!(
                "[providers.fi.models.m.price]\n",
                "input = 1.0\n",
                "output = 2.0\n",
                "cached = 0.1\n",
            ),
        );
        let merged = settings(Some(&global), None);
        let price = merged.providers["fi"].models["m"]
            .price
            .expect("a price table");
        assert_eq!(price.input, 1.0);
        assert_eq!(price.output, 2.0);
        assert_eq!(price.cached, 0.1);
        assert_eq!(price.cache_write, None);

        let written = write(
            dir.path(),
            "full.toml",
            concat!(
                "[providers.fi.models.m.price]\n",
                "input = 1.0\n",
                "output = 2.0\n",
                "cached = 0.1\n",
                "cache_write = 1.25\n",
            ),
        );
        let merged = settings(Some(&written), None);
        assert_eq!(
            merged.providers["fi"].models["m"].price,
            Some(ModelPrice {
                input: 1.0,
                output: 2.0,
                cached: 0.1,
                cache_write: Some(1.25),
            })
        );
    }

    #[test]
    fn price_rule_a_negative_or_non_finite_config_price_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        for amount in ["-1.0", "inf", "nan"] {
            let path = write(
                dir.path(),
                "bad.toml",
                &format!(
                    "[providers.fi.models.m.price]\ninput = 1.0\noutput = 2.0\ncached = {amount}\n"
                ),
            );
            let error = load(Some(&path), None).expect_err("a bad price is an error");
            assert!(error.contains("finite amount"), "{error}");
        }
        let path = write(
            dir.path(),
            "unknown.toml",
            "[providers.fi.models.m.price]\ninput = 1.0\noutput = 2.0\ncached = 0.1\nbogus = 1\n",
        );
        assert!(
            load(Some(&path), None).is_err(),
            "an unknown price key fails"
        );
    }

    #[test]
    fn an_unknown_provider_key_names_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            "[providers.fi]\nbaseurl = \"x\"\n",
        );
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains("baseurl"), "{error}");
    }

    #[test]
    fn an_old_top_level_provider_key_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "base_url = \"http://x\"\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("base_url"), "{error}");
    }

    #[test]
    fn an_unknown_key_names_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "modle = \"glm\"\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains("modle"), "{error}");
    }

    #[test]
    fn layers_keep_their_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "policy = [\"all\"]\n");
        let layers = load(Some(&global), None).expect("loads");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].path, global);
        assert_eq!(layers[0].settings.policy, Some(vec!["all".to_string()]));
    }

    /// The merged settings of a global file, a profile file and a repo file.
    fn with_profile(
        global: Option<&Path>,
        profile: Option<&Path>,
        repo: Option<&Path>,
    ) -> Settings {
        let layers = Layers {
            global: load_file(global, false).expect("loads"),
            profile: profile
                .map(|path| load_file(Some(path), false))
                .transpose()
                .expect("loads")
                .flatten(),
            repo: load_file(repo, true).expect("loads"),
            saved: None,
        };
        merge_all(&layers)
    }

    #[test]
    fn profile_layer_the_profile_model_beats_the_global_and_the_repo_beats_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "model = \"glm\"\n");
        let profile = write(dir.path(), "profiles/work.toml", "model = \"kimi\"\n");
        assert_eq!(
            with_profile(Some(&global), Some(&profile), None)
                .model
                .as_deref(),
            Some("kimi")
        );
        let repo = write(dir.path(), "repo.toml", "model = \"repo\"\n");
        assert_eq!(
            with_profile(Some(&global), Some(&profile), Some(&repo))
                .model
                .as_deref(),
            Some("repo")
        );
    }

    #[test]
    fn profile_layer_a_provider_only_in_the_profile_is_used() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "");
        let profile = write(
            dir.path(),
            "profiles/work.toml",
            "[providers.work]\nbase_url = \"https://work.example/v1\"\n",
        );
        let merged = with_profile(Some(&global), Some(&profile), None);
        assert_eq!(
            merged.providers["work"].base_url.as_deref(),
            Some("https://work.example/v1")
        );
    }

    #[test]
    fn profile_layer_a_table_in_both_merges_field_by_field() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            concat!(
                "[providers.fi]\n",
                "base_url = \"https://fi.example/v1\"\n",
                "[providers.fi.headers]\n",
                "x-team = \"global\"\n",
                "x-both = \"global\"\n",
                "[providers.fi.models.\"glm\"]\n",
                "max_tokens = 100\n",
            ),
        );
        let profile = write(
            dir.path(),
            "profiles/work.toml",
            concat!(
                "[providers.fi]\n",
                "api_key_env = \"WORK_KEY\"\n",
                "[providers.fi.headers]\n",
                "x-both = \"profile\"\n",
                "[providers.fi.models.\"glm\"]\n",
                "context_window = 9000\n",
            ),
        );
        let merged = with_profile(Some(&global), Some(&profile), None);
        let table = &merged.providers["fi"];
        // The field the profile sets replaces the global one, and the global
        // base URL stays.
        assert_eq!(table.api_key_env.as_deref(), Some("WORK_KEY"));
        assert_eq!(table.base_url.as_deref(), Some("https://fi.example/v1"));
        assert_eq!(table.headers["x-team"], "global");
        assert_eq!(table.headers["x-both"], "profile");
        assert_eq!(table.models["glm"].max_tokens, Some(100));
        assert_eq!(table.models["glm"].context_window, Some(9000));
    }

    #[test]
    fn profile_layer_a_missing_profile_names_the_folder() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = load_profile_in(dir.path(), "work").unwrap_err();
        assert_eq!(
            error,
            format!("no profile work in {}/profiles", dir.path().display())
        );
    }

    #[test]
    fn profile_layer_a_bad_profile_name_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        for name in ["a/b", "..", "a.b", "-work", ""] {
            let error = load_profile_in(dir.path(), name).unwrap_err();
            assert!(error.contains("invalid profile"), "{name}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn profile_layer_a_shared_profile_file_may_not_hold_an_api_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = write(
            dir.path(),
            "profiles/work.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        chmod(&profile, 0o644);
        let error = load_profile_in(dir.path(), "work").unwrap_err();
        assert!(error.contains("chmod 600"), "{error}");
        assert!(!error.contains("sk-test-123"), "{error}");
    }

    #[test]
    fn profile_layer_the_repo_file_still_cannot_add_providers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = write(
            dir.path(),
            "repo.toml",
            "[providers.work]\nbase_url = \"http://evil\"\n",
        );
        let error = load(None, Some(&repo)).unwrap_err();
        assert!(error.contains("providers"), "{error}");
    }

    #[test]
    fn no_files_means_no_settings() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing.toml");
        assert!(
            load(Some(&missing), Some(&missing))
                .expect("loads")
                .is_empty()
        );
        assert_eq!(settings(None, None), Settings::default());
    }

    #[test]
    fn repo_root_finds_the_folder_with_git() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "project/.git", "gitdir: elsewhere\n");
        write(dir.path(), "project/a/b/file", "");
        let nested = dir.path().join("project/a/b");
        assert_eq!(repo_root(&nested), Some(dir.path().join("project")));
    }

    #[test]
    fn the_ui_setting_reads_full_or_inline_and_names_itself_on_a_typo() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = write(dir.path(), "config.toml", "ui = \"inline\"\n");
        let found = settings(Some(&file), None);
        assert_eq!(found.ui(), Ok(Some(Ui::Inline)));
        let file = write(dir.path(), "config.toml", "ui = \"bogus\"\n");
        let found = settings(Some(&file), None);
        let error = found.ui().expect_err("an unknown value is an error");
        assert!(error.contains("ui"), "{error}");
        assert!(error.contains("bogus"), "{error}");
    }

    #[test]
    fn the_theme_setting_is_a_path_and_a_later_layer_wins() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "config.toml", "theme = \"one.toml\"\n");
        let found = settings(Some(&global), None);
        assert_eq!(found.theme(), Some(Path::new("one.toml")));
        let repo = write(dir.path(), "repo.toml", "theme = \"two.toml\"\n");
        let found = settings(Some(&global), Some(&repo));
        assert_eq!(found.theme(), Some(Path::new("two.toml")));
        let empty = write(dir.path(), "empty.toml", "theme = \"\"\n");
        assert_eq!(settings(Some(&empty), None).theme(), None);
    }

    #[test]
    fn the_global_dir_follows_xdg_then_home() {
        let some = |text: &str| Some(OsString::from(text));
        assert_eq!(
            global_dir_from(some("/x"), some("/h")),
            Some(PathBuf::from("/x/reagent"))
        );
        assert_eq!(
            global_dir_from(some(""), some("/h")),
            Some(PathBuf::from("/h/.config/reagent"))
        );
        assert_eq!(global_dir_from(None, None), None);
    }
}
