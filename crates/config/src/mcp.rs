//! MCP servers read from `mcp.toml` and a profile's `<name>.mcp.toml`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize, Serializer};

use crate::Secret;

/// The seconds one call may take when the entry sets no `timeout`.
pub const DEFAULT_TIMEOUT: u64 = 60;

/// The first line of every file `save_mcp` writes.
pub const HEADER: &str = "# Written by reagent mcp; edit with reagent mcp add or by hand.\n";

const FILE: &str = "mcp.toml";

/// How a server's tools reach the request as tools of their own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum McpDirect {
    /// No tool is offered directly.
    #[default]
    None,
    /// Every tool is offered directly.
    All,
    /// Only these tools, by their MCP names, are offered directly.
    Only(Vec<String>),
}

/// How a server is reached.
#[derive(Clone, PartialEq)]
pub enum McpTransport {
    /// A child process speaking MCP on stdin and stdout.
    Stdio {
        /// The program to run.
        command: String,
        /// Its arguments, in order.
        args: Vec<String>,
        /// Extra environment variables in file order, unexpanded.
        env: Vec<(String, String)>,
        /// The folder the process runs in, or the agent's when absent.
        cwd: Option<PathBuf>,
    },
    /// A streamable HTTP endpoint.
    Http {
        /// The endpoint URL.
        url: String,
        /// Extra request headers in file order, their values unexpanded.
        headers: Vec<(String, String)>,
    },
}

/// One MCP server of the config.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServer {
    /// How the server is reached.
    pub transport: McpTransport,
    /// How long one call may take.
    pub timeout: Duration,
    /// Whether a run starts it unless the pick list says otherwise.
    pub enabled: bool,
    /// Which of its tools the request offers as tools of their own.
    pub direct: McpDirect,
    /// The login settings of an HTTP server, when it has them.
    pub oauth: Option<McpOAuth>,
}

/// The names of a pair list, without the values, which may hold secrets.
struct Names<'a>(&'a [(String, String)]);

impl fmt::Debug for Names<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                args,
                env,
                cwd,
            } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &Names(env))
                .field("cwd", cwd)
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &Names(headers))
                .finish(),
        }
    }
}

/// A `[servers.<name>.oauth]` table: the login of an HTTP server.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpOAuth {
    /// A client id registered by hand, which skips registration.
    pub client_id: Option<String>,
    /// The secret of that client.
    pub client_secret: Option<Secret>,
    /// A client id metadata document, which is itself the `client_id`.
    pub client_metadata_url: Option<String>,
    /// The port the callback listens on; any free one when absent.
    pub callback_port: Option<u16>,
    /// The scopes to ask for, in place of the resource's own.
    pub scope: Option<String>,
}

/// One `[servers.<name>]` entry as written, before the checks.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default, deserialize_with = "ordered_pairs")]
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    url: Option<String>,
    #[serde(default, deserialize_with = "ordered_pairs")]
    headers: Vec<(String, String)>,
    timeout: Option<u64>,
    enabled: Option<bool>,
    /// `true`, `false`, or a list of tool names, checked by `server`.
    direct: Option<toml::Value>,
    oauth: Option<McpOAuth>,
}

/// A whole server file: `[servers.<name>]` tables and one `[settings]` table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    #[serde(default)]
    servers: BTreeMap<String, RawServer>,
    #[serde(default)]
    settings: RawSettings,
}

/// The `[settings]` table of a server file.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    /// The `direct` an entry that sets none takes.
    direct: Option<toml::Value>,
    /// Whether a run warns when many tools are direct.
    warn_many_direct: Option<bool>,
}

/// The `direct` a `true`, `false` or list value sets, as `where_` names it.
fn direct_value(value: &toml::Value, where_: &str) -> Result<McpDirect, String> {
    match value {
        toml::Value::Boolean(true) => Ok(McpDirect::All),
        toml::Value::Boolean(false) => Ok(McpDirect::None),
        toml::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    format!("{where_}: direct takes true, false or a list of tool names")
                })
            })
            .collect::<Result<Vec<String>, String>>()
            .map(McpDirect::Only),
        _ => Err(format!(
            "{where_}: direct takes true, false or a list of tool names"
        )),
    }
}

/// Reads a table of strings as `(name, value)` pairs in file order.
fn ordered_pairs<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let table = BTreeMap::<toml::Spanned<String>, String>::deserialize(deserializer)?;
    let mut pairs: Vec<_> = table.into_iter().collect();
    pairs.sort_by_key(|(name, _)| name.span().start);
    Ok(pairs
        .into_iter()
        .map(|(name, value)| (name.into_inner(), value))
        .collect())
}

impl RawServer {
    /// The checked server `name` this entry describes, and the `direct` it sets
    /// itself, before the file's default applies.
    fn server(self, name: &str) -> Result<(McpServer, Option<McpDirect>), String> {
        let direct = match &self.direct {
            Some(value) => Some(direct_value(value, &format!("server {name}"))?),
            None => None,
        };
        let transport = match (self.command, self.url) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "server {name}: set either command or url, not both"
                ));
            }
            (None, None) => return Err(format!("server {name}: set command or url")),
            (Some(command), None) => {
                if self.oauth.is_some() {
                    return Err(format!(
                        "server {name}: oauth is allowed only on a server with a url"
                    ));
                }
                McpTransport::Stdio {
                    command,
                    args: self.args,
                    env: self.env,
                    cwd: self.cwd,
                }
            }
            (None, Some(url)) => McpTransport::Http {
                url,
                headers: self.headers,
            },
        };
        Ok((
            McpServer {
                transport,
                timeout: Duration::from_secs(self.timeout.unwrap_or(DEFAULT_TIMEOUT)),
                enabled: self.enabled.unwrap_or(true),
                direct: McpDirect::None,
                oauth: self.oauth,
            },
            direct,
        ))
    }
}

/// One entry as the file writes it.
#[derive(Serialize)]
struct Written<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<&'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct: Option<WrittenDirect<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth: Option<&'a McpOAuth>,
}

/// The `direct` of one entry as the file writes it.
#[derive(Serialize)]
#[serde(untagged)]
enum WrittenDirect<'a> {
    /// `direct = true` or `direct = false`.
    All(bool),
    /// `direct = ["search", "get_issue"]`.
    Only(Vec<&'a str>),
}

impl<'a> Written<'a> {
    /// `server` as the file writes it, under a file whose `[settings]` make
    /// `default` the `direct` of an entry that sets none.
    fn of(server: &'a McpServer, default: &McpDirect) -> Written<'a> {
        let pairs = |list: &'a [(String, String)]| {
            list.iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect()
        };
        let timeout = server.timeout.as_secs();
        let (command, args, env, cwd, url, headers) = match &server.transport {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => (
                Some(command.as_str()),
                args.iter().map(String::as_str).collect(),
                pairs(env),
                cwd.as_deref(),
                None,
                BTreeMap::new(),
            ),
            McpTransport::Http { url, headers } => (
                None,
                Vec::new(),
                BTreeMap::new(),
                None,
                Some(url.as_str()),
                pairs(headers),
            ),
        };
        // An entry whose `direct` is the file's default writes no key, so a
        // later change of the default reaches it.
        let direct = if server.direct == *default {
            None
        } else {
            Some(match &server.direct {
                McpDirect::None => WrittenDirect::All(false),
                McpDirect::All => WrittenDirect::All(true),
                McpDirect::Only(names) => {
                    WrittenDirect::Only(names.iter().map(String::as_str).collect())
                }
            })
        };
        Written {
            command,
            args,
            env,
            cwd,
            url,
            headers,
            timeout: (timeout != DEFAULT_TIMEOUT).then_some(timeout),
            // An absent `enabled` means on, so only an off server writes one.
            enabled: (!server.enabled).then_some(false),
            direct,
            oauth: server.oauth.as_ref(),
        }
    }
}

/// One `oauth` table as the file writes it.
#[derive(Serialize)]
struct WrittenOAuth<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_metadata_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

impl<'a> WrittenOAuth<'a> {
    /// `oauth` as the file writes it, its secret exposed for this write only.
    fn of(oauth: &'a McpOAuth) -> WrittenOAuth<'a> {
        WrittenOAuth {
            client_id: oauth.client_id.as_deref(),
            client_secret: oauth.client_secret.as_ref().map(Secret::expose),
            client_metadata_url: oauth.client_metadata_url.as_deref(),
            callback_port: oauth.callback_port,
            scope: oauth.scope.as_deref(),
        }
    }
}

impl Serialize for McpOAuth {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WrittenOAuth::of(self).serialize(serializer)
    }
}

/// A whole file: the `[settings]` table when it sets a key, then the
/// `[servers.<name>]` tables.
#[derive(Serialize)]
struct WrittenFile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    settings: Option<WrittenSettings<'a>>,
    servers: BTreeMap<&'a str, Written<'a>>,
}

/// The `[settings]` table as the file writes it.
#[derive(Serialize)]
struct WrittenSettings<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    direct: Option<WrittenDirect<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warn_many_direct: Option<bool>,
}

/// Writes `servers` to `path` under `settings`, sorted by name, readable by
/// you only.
pub fn save_mcp(
    path: &Path,
    servers: &BTreeMap<String, McpServer>,
    settings: &McpSettings,
) -> Result<(), String> {
    let default = settings.direct.clone().unwrap_or_default();
    let written = (settings.direct.is_some() || settings.warn_many_direct.is_some()).then(|| {
        WrittenSettings {
            direct: settings.direct.as_ref().map(|direct| match direct {
                McpDirect::None => WrittenDirect::All(false),
                McpDirect::All => WrittenDirect::All(true),
                McpDirect::Only(names) => {
                    WrittenDirect::Only(names.iter().map(String::as_str).collect())
                }
            }),
            warn_many_direct: settings.warn_many_direct,
        }
    });
    let file = WrittenFile {
        settings: written,
        servers: servers
            .iter()
            .map(|(name, server)| (name.as_str(), Written::of(server, &default)))
            .collect(),
    };
    let text = toml::to_string(&file)
        .map_err(|error| format!("{}: cannot write the servers: {error}", path.display()))?;
    write(path, &format!("{HEADER}{text}"))
}

/// Replaces `path` with `text`: a temp file in its folder, mode 0600, rename.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "the server file is written the way auth.json is: temp file, 0600, rename"
)]
fn write(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;

    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    // The folder may hold a client secret or a header value.
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(folder)
        .map_err(|error| write_failed(path, &error))?;
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp-{}", std::process::id()));
    let temp = PathBuf::from(temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| write_failed(&temp, &error))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| write_failed(path, &error))?;
    drop(file);
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        write_failed(path, &error)
    })
}

/// The path and the reason a write failed.
fn write_failed(path: &Path, error: &std::io::Error) -> String {
    format!("{}: {error}", path.display())
}

/// Whether `name` can name a server: lowercase letters, digits, `-` and `_`.
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// One MCP server as an editor sends it in an ACP `session/new` request, before
/// the checks.
#[derive(Deserialize)]
struct RawSpec {
    /// `stdio` when absent or named, `http` or `sse`.
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<Vec<SpecPair>>,
    url: Option<String>,
    #[serde(default)]
    headers: Option<Vec<SpecPair>>,
}

/// One `{name, value}` pair of a spec.
#[derive(Deserialize)]
struct SpecPair {
    name: String,
    value: String,
}

impl RawSpec {
    /// The `(name, value)` pairs of `pairs`, in the order given.
    fn pairs(pairs: Option<Vec<SpecPair>>) -> Vec<(String, String)> {
        pairs
            .unwrap_or_default()
            .into_iter()
            .map(|pair| (pair.name, pair.value))
            .collect()
    }
}

/// Reads one `--mcp-spec` value as a server and the name it takes, with the
/// defaults an `mcp.toml` entry gets.
///
/// The JSON is the stdio shape an editor sends in an ACP `session/new` request,
/// `{"name", "command", "args", "env": [{"name", "value"}]}`, or the http shape
/// `{"type": "http", "name", "url", "headers": [{"name", "value"}]}`. `sse` has
/// no transport here, so it is refused. Nothing here is stored, so no error and
/// no `Debug` output holds an env or a header value.
pub fn server_from_spec(text: &str) -> Result<(String, McpServer), String> {
    let spec: RawSpec =
        serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))?;
    let name = spec.name.ok_or_else(|| "missing name".to_string())?;
    if !valid_server_name(&name) {
        return Err(format!(
            "invalid name {name}: use lowercase letters, digits, - and _ only"
        ));
    }
    let transport = match spec.kind.as_deref() {
        None | Some("stdio") => {
            if spec.command.is_some() && spec.url.is_some() {
                return Err("set either command or url, not both".to_string());
            }
            let command = spec.command.ok_or_else(|| "missing command".to_string())?;
            if !Path::new(&command).is_absolute() {
                return Err(format!("command is not an absolute path: {command}"));
            }
            McpTransport::Stdio {
                command,
                args: spec.args.unwrap_or_default(),
                env: RawSpec::pairs(spec.env),
                cwd: None,
            }
        }
        Some("http") => McpTransport::Http {
            url: spec.url.ok_or_else(|| "missing url".to_string())?,
            headers: RawSpec::pairs(spec.headers),
        },
        Some("sse") => {
            return Err("type sse is not supported: pass a stdio or an http server".to_string());
        }
        Some(other) => {
            return Err(format!(
                "unknown type {other}: use a stdio or an http server"
            ));
        }
    };
    Ok((
        name,
        McpServer {
            transport,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT),
            enabled: true,
            direct: McpDirect::None,
            oauth: None,
        },
    ))
}

/// The servers of a config folder and the settings they share.
#[derive(Clone, Debug, PartialEq)]
pub struct McpConfig {
    /// The servers by name, their `direct` resolved.
    pub servers: BTreeMap<String, McpServer>,
    /// Whether a run warns when many tools are direct.
    pub warn_many_direct: bool,
    /// The `[settings]` keys as the files set them, for a rewrite to keep.
    pub settings: McpSettings,
}

/// The `[settings]` table of a server file, each key absent when unset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpSettings {
    /// The `direct` an entry that sets none takes.
    pub direct: Option<McpDirect>,
    /// Whether a run warns when many tools are direct.
    pub warn_many_direct: Option<bool>,
}

/// One server file as written, before the settings fill each entry.
#[derive(Default)]
struct Read {
    /// Each server and the `direct` its own entry sets.
    servers: BTreeMap<String, (McpServer, Option<McpDirect>)>,
    /// The `direct` an entry that sets none takes.
    direct: Option<McpDirect>,
    /// Whether a run warns when many tools are direct.
    warn_many_direct: Option<bool>,
}

impl Read {
    /// The config these entries and settings make, every `direct` resolved.
    fn resolved(self) -> McpConfig {
        let direct = self.direct.clone().unwrap_or_default();
        let servers = self
            .servers
            .into_iter()
            .map(|(name, (mut server, own))| {
                server.direct = own.unwrap_or_else(|| direct.clone());
                (name, server)
            })
            .collect();
        McpConfig {
            servers,
            warn_many_direct: self.warn_many_direct.unwrap_or(true),
            settings: McpSettings {
                direct: self.direct,
                warn_many_direct: self.warn_many_direct,
            },
        }
    }
}

/// Reads one server file, or an empty one when it is absent.
fn read_file(path: &Path) -> Result<Read, String> {
    let Some(file) = crate::read_text(path)? else {
        return Ok(Read::default());
    };
    let text = file.text;
    let entries: ServerFile =
        toml::from_str(&text).map_err(|error| crate::parse_error(path, &text, &error))?;
    if file.shared
        && let Some(field) = exposed(&entries)
    {
        return Err(format!(
            "{0}: holds {field}, so only you may read it; run chmod 600 {0}",
            path.display()
        ));
    }
    let settings = entries.settings;
    let direct = settings
        .direct
        .as_ref()
        .map(|value| direct_value(value, "[settings]"))
        .transpose()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let mut servers = BTreeMap::new();
    for (name, entry) in entries.servers {
        if !valid_server_name(&name) {
            return Err(format!(
                "{}: invalid server name {name}: use lowercase letters, digits, - and _ only",
                path.display()
            ));
        }
        let (server, own) = entry
            .server(&name)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        servers.insert(name, (server, own));
    }
    Ok(Read {
        servers,
        direct,
        warn_many_direct: settings.warn_many_direct,
    })
}

/// The servers of one file, their `direct` resolved against its settings.
pub fn load_mcp_file(path: &Path) -> Result<McpConfig, String> {
    read_file(path).map(Read::resolved)
}

/// The first field of one server entry that names a secret a shared file may
/// not hold: a `client_secret`, an `env` value or a header value that is not
/// only a `${VAR}` reference.
fn exposed(entries: &ServerFile) -> Option<&'static str> {
    for entry in entries.servers.values() {
        let secret = entry
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.client_secret.as_ref());
        if secret.is_some_and(|secret| !only_variable(secret.expose())) {
            return Some("a client_secret");
        }
        if entry.env.iter().any(|(_, value)| !only_variable(value)) {
            return Some("an env value");
        }
        if entry.headers.iter().any(|(_, value)| !only_variable(value)) {
            return Some("a header value");
        }
    }
    None
}

/// Whether `value` holds nothing but a `${VAR}` reference, which is no secret
/// itself.
fn only_variable(value: &str) -> bool {
    let Some(name) = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return false;
    };
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The file of the config folder `dir` that the servers of `profile` go in.
pub fn file(dir: &Path, profile: Option<&str>) -> Result<PathBuf, String> {
    let Some(name) = profile else {
        return Ok(dir.join(FILE));
    };
    if !crate::valid_name(name) {
        return Err(format!(
            "invalid profile {name}: use letters, digits, - and _ only, not starting with -"
        ));
    }
    Ok(dir.join("profiles").join(format!("{name}.{FILE}")))
}

/// The servers of the config folder `dir`, with the ones of the profile
/// `profile` over them by name.
pub fn load_mcp(dir: &Path, profile: Option<&str>) -> Result<McpConfig, String> {
    let mut read = read_file(&dir.join(FILE))?;
    if profile.is_some() {
        let over = read_file(&file(dir, profile)?)?;
        // A profile's settings win key by key, and its servers by name.
        read.direct = over.direct.or(read.direct);
        read.warn_many_direct = over.warn_many_direct.or(read.warn_many_direct);
        read.servers.extend(over.servers);
    }
    Ok(read.resolved())
}

/// The servers of the global config folder, with the ones of the profile
/// `profile` over them by name.
pub fn load_mcp_for(profile: Option<&str>) -> Result<McpConfig, String> {
    match crate::global_dir() {
        Some(dir) => load_mcp(&dir, profile),
        None => Ok(Read::default().resolved()),
    }
}

#[cfg(test)]
mod tests {
    use super::load_mcp;
    use std::path::{Path, PathBuf};

    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("makes the folder");
        std::fs::write(&path, text).expect("writes the file");
        mode(&path, PRIVATE);
        path
    }

    /// A file only its owner may read.
    const PRIVATE: u32 = 0o600;

    /// A file group and others may read.
    const SHARED: u32 = 0o644;

    /// The mode bits of `path`, which decide whether a secret may be in it.
    #[cfg(unix)]
    fn mode(path: &Path, bits: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
            .expect("sets the mode");
    }

    #[cfg(not(unix))]
    fn mode(_path: &Path, _bits: u32) {}

    #[cfg(unix)]
    #[test]
    fn mcp_config_a_shared_file_holds_no_literal_secret() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (text, field) in [
            (
                concat!(
                    "[servers.docs]\n",
                    "url = \"https://example.com/mcp\"\n",
                    "[servers.docs.oauth]\n",
                    "client_secret = \"hunter2\"\n",
                ),
                "a client_secret",
            ),
            (
                "[servers.fs]\ncommand = \"npx\"\nenv = { KEY = \"hunter2\" }\n",
                "an env value",
            ),
            (
                concat!(
                    "[servers.docs]\n",
                    "url = \"https://example.com/mcp\"\n",
                    "headers = { Authorization = \"Bearer hunter2\" }\n",
                ),
                "a header value",
            ),
        ] {
            let path = write(dir.path(), "mcp.toml", text);
            mode(&path, SHARED);
            let error = load_mcp(dir.path(), None).unwrap_err();
            assert!(error.contains("only you may read it"), "{text}: {error}");
            assert!(error.contains("run chmod 600"), "{text}: {error}");
            assert!(error.contains("mcp.toml"), "{text}: {error}");
            assert!(!error.contains("hunter2"), "{text}: {error}");
            assert!(error.contains(field), "{text}: {error}");
        }
        // A profile file is held to the same rule.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write(
            dir.path(),
            "profiles/work.mcp.toml",
            "[servers.fs]\ncommand = \"npx\"\nenv = { KEY = \"hunter2\" }\n",
        );
        mode(&path, SHARED);
        let error = load_mcp(dir.path(), Some("work")).unwrap_err();
        assert!(error.contains("work.mcp.toml"), "{error}");
        assert!(error.contains("only you may read it"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn mcp_config_a_shared_file_of_variable_references_is_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write(
            dir.path(),
            "mcp.toml",
            concat!(
                "[servers.fs]\n",
                "command = \"npx\"\n",
                "env = { KEY = \"${KEY}\" }\n",
                "\n",
                "[servers.docs]\n",
                "url = \"https://example.com/mcp\"\n",
                "headers = { Authorization = \"${TOKEN}\" }\n",
                "\n",
                "[servers.docs.oauth]\n",
                "client_secret = \"${SECRET}\"\n",
            ),
        );
        mode(&path, SHARED);
        let servers = load_mcp(dir.path(), None).expect("loads").servers;
        assert_eq!(servers.len(), 2);
    }

    const BOTH: &str = concat!(
        "[servers.fs]\n",
        "command = \"npx\"\n",
        "args = [\"-y\", \"@modelcontextprotocol/server-filesystem\", \".\"]\n",
        "env = { ZED = \"${TOOLS_KEY}\", API_KEY = \"${KEY}\" }\n",
        "cwd = \"/some/dir\"\n",
        "timeout = 30\n",
        "direct = [\"read_file\"]\n",
        "\n",
        "[servers.docs]\n",
        "url = \"https://example.com/mcp\"\n",
        "headers = { Authorization = \"Bearer ${DOCS_TOKEN}\", Accept = \"application/json\" }\n",
        "\n",
        "[servers.docs.oauth]\n",
        "client_id = \"client-1\"\n",
        "client_secret = \"${DOCS_SECRET}\"\n",
        "client_metadata_url = \"https://example.com/client.json\"\n",
        "callback_port = 8765\n",
        "scope = \"read write\"\n",
    );

    #[test]
    fn mcp_config_reads_stdio_and_http() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "mcp.toml", BOTH);
        let servers = load_mcp(dir.path(), None).expect("loads").servers;
        let names: Vec<&str> = servers.keys().map(String::as_str).collect();
        assert_eq!(names, ["docs", "fs"]);
        let fs = &servers["fs"];
        assert_eq!(
            fs.transport,
            super::McpTransport::Stdio {
                command: "npx".into(),
                args: ["-y", "@modelcontextprotocol/server-filesystem", "."]
                    .map(str::to_string)
                    .to_vec(),
                env: [
                    ("ZED".to_string(), "${TOOLS_KEY}".to_string()),
                    ("API_KEY".to_string(), "${KEY}".to_string()),
                ]
                .to_vec(),
                cwd: Some(PathBuf::from("/some/dir")),
            }
        );
        assert_eq!(fs.timeout, std::time::Duration::from_secs(30));
        assert!(fs.enabled);
        assert_eq!(fs.oauth, None);
        let docs = &servers["docs"];
        assert_eq!(
            docs.transport,
            super::McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: [
                    (
                        "Authorization".to_string(),
                        "Bearer ${DOCS_TOKEN}".to_string()
                    ),
                    ("Accept".to_string(), "application/json".to_string()),
                ]
                .to_vec(),
            }
        );
        // The entry sets neither, so the defaults hold.
        assert_eq!(docs.timeout, std::time::Duration::from_secs(60));
        assert!(docs.enabled);
        let oauth = docs.oauth.clone().expect("an oauth table");
        assert_eq!(oauth.client_id.as_deref(), Some("client-1"));
        let secret = oauth.client_secret.expect("a client secret");
        assert_eq!(secret.expose(), "${DOCS_SECRET}");
        assert_eq!(
            oauth.client_metadata_url.as_deref(),
            Some("https://example.com/client.json")
        );
        assert_eq!(oauth.callback_port, Some(8765));
        assert_eq!(oauth.scope.as_deref(), Some("read write"));
    }

    #[test]
    fn mcp_config_debug_hides_env_and_header_values() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "mcp.toml", BOTH);
        let servers = load_mcp(dir.path(), None).expect("loads").servers;
        let text = format!("{servers:?}");
        for value in ["${TOOLS_KEY}", "${KEY}", "${DOCS_TOKEN}", "${DOCS_SECRET}"] {
            assert!(!text.contains(value), "{text}");
        }
        assert!(text.contains("ZED"), "{text}");
        assert!(text.contains("Authorization"), "{text}");
    }

    #[test]
    fn mcp_config_profile_replaces_by_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(
            dir.path(),
            "mcp.toml",
            concat!(
                "[servers.gh]\n",
                "command = \"gh-mcp\"\n",
                "args = [\"global\"]\n",
                "timeout = 5\n",
                "\n",
                "[servers.other]\n",
                "command = \"other\"\n",
            ),
        );
        write(
            dir.path(),
            "profiles/work.mcp.toml",
            "[servers.gh]\ncommand = \"gh-mcp\"\nargs = [\"profile\"]\n",
        );
        let profile = load_mcp(dir.path(), Some("work")).expect("loads").servers;
        let names: Vec<&str> = profile.keys().map(String::as_str).collect();
        assert_eq!(names, ["gh", "other"]);
        assert_eq!(profile["gh"].transport, stdio_args(&["profile"]));
        // The whole entry is replaced, so the global timeout is gone too.
        assert_eq!(profile["gh"].timeout, std::time::Duration::from_secs(60));
        assert_eq!(
            profile["other"].transport,
            super::McpTransport::Stdio {
                command: "other".into(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: None,
            }
        );
        let global = load_mcp(dir.path(), None).expect("loads").servers;
        assert_eq!(global["gh"].transport, stdio_args(&["global"]));
        assert_eq!(global["gh"].timeout, std::time::Duration::from_secs(5));
    }

    fn stdio_args(args: &[&str]) -> super::McpTransport {
        super::McpTransport::Stdio {
            command: "gh-mcp".into(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// The servers with every pair list sorted, since a written table is.
    fn pairs_sorted(
        servers: &std::collections::BTreeMap<String, super::McpServer>,
    ) -> Vec<(String, super::McpServer)> {
        servers
            .iter()
            .map(|(name, server)| {
                let mut server = server.clone();
                match &mut server.transport {
                    super::McpTransport::Stdio { env, .. } => env.sort(),
                    super::McpTransport::Http { headers, .. } => headers.sort(),
                }
                (name.clone(), server)
            })
            .collect()
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn mcp_config_save_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let original = write(dir.path(), "mcp.toml", BOTH);
        let servers = load_mcp(dir.path(), None).expect("loads").servers;
        let path = dir.path().join("written.toml");
        super::save_mcp(&path, &servers, &super::McpSettings::default()).expect("writes");
        let text = std::fs::read_to_string(&path).expect("reads");
        assert!(text.starts_with(super::HEADER), "{text}");
        assert!(text.contains("[servers.docs]"), "{text}");
        let back = super::load_mcp_file(&path).expect("reads back").servers;
        assert_eq!(pairs_sorted(&back), pairs_sorted(&servers), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("the file")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The original file is untouched, and an empty table writes a header.
        assert_eq!(std::fs::read_to_string(&original).expect("reads"), BOTH);
        super::save_mcp(
            &path,
            &super::BTreeMap::new(),
            &super::McpSettings::default(),
        )
        .expect("writes");
        let empty = std::fs::read_to_string(&path).expect("reads");
        assert!(empty.starts_with(super::HEADER), "{empty}");
        assert!(
            super::load_mcp_file(&path)
                .expect("reads back")
                .servers
                .is_empty()
        );
    }

    #[test]
    fn mcp_config_file_names_the_profile() {
        let dir = Path::new("/config");
        assert_eq!(
            super::file(dir, None).expect("the global file"),
            PathBuf::from("/config/mcp.toml")
        );
        assert_eq!(
            super::file(dir, Some("work")).expect("the profile file"),
            PathBuf::from("/config/profiles/work.mcp.toml")
        );
        let error = super::file(dir, Some("-bad")).unwrap_err();
        assert!(error.contains("invalid profile"), "{error}");
    }

    #[test]
    fn mcp_config_bad_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (text, fragments) in [
            (
                "[servers.fs]\ncommand = \"npx\"\nurl = \"https://x/mcp\"\n",
                &["server fs"][..],
            ),
            ("[servers.fs]\nargs = [\"-y\"]\n", &["server fs"][..]),
            (
                concat!(
                    "[servers.fs]\n",
                    "command = \"npx\"\n",
                    "[servers.fs.oauth]\n",
                    "client_id = \"c\"\n",
                ),
                &["server fs", "oauth"][..],
            ),
            ("[servers.FS]\ncommand = \"npx\"\n", &["FS"][..]),
            ("[servers.fs]\ncommand = \"npx\"\nnope = 1\n", &["nope"][..]),
        ] {
            write(dir.path(), "mcp.toml", text);
            let error = load_mcp(dir.path(), None).unwrap_err();
            assert!(error.contains("mcp.toml"), "{text}: {error}");
            for fragment in fragments {
                assert!(error.contains(fragment), "{text}: {error}");
            }
        }
    }

    #[test]
    fn mcp_config_servers_not_in_config_toml() {
        let dir = tempfile::tempdir().expect("temp dir");
        for key in ["mcp_servers", "servers"] {
            let repo = write(
                dir.path(),
                "config.toml",
                &format!("[{key}.x]\ncommand = \"npx\"\n"),
            );
            let error = crate::load_file(Some(&repo), true).unwrap_err();
            assert!(
                error.contains("servers belong in mcp.toml"),
                "{key}: {error}"
            );
            assert!(error.contains("config.toml"), "{key}: {error}");
            let global = write(dir.path(), "global.toml", &format!("[{key}.x]\n"));
            let error = crate::load_file(Some(&global), false).unwrap_err();
            assert!(
                error.contains("servers belong in mcp.toml"),
                "{key}: {error}"
            );
        }
        // The pick list itself is allowed in every layer, the repo file included.
        let repo = write(dir.path(), "config.toml", "mcp = [\"-one\", \"+two\"]\n");
        let layer = crate::load_file(Some(&repo), true)
            .expect("a list is safe")
            .expect("a layer");
        assert_eq!(layer.settings.mcp, Some(vec!["-one".into(), "+two".into()]));
    }

    #[test]
    fn mcp_direct_config_values() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(
            dir.path(),
            "mcp.toml",
            concat!(
                "[servers.all]\n",
                "command = \"a\"\n",
                "direct = true\n",
                "\n",
                "[servers.none]\n",
                "command = \"b\"\n",
                "direct = false\n",
                "\n",
                "[servers.some]\n",
                "command = \"c\"\n",
                "direct = [\"search\", \"get_issue\"]\n",
            ),
        );
        let loaded = load_mcp(dir.path(), None).expect("loads");
        assert_eq!(loaded.servers["all"].direct, super::McpDirect::All);
        assert_eq!(loaded.servers["none"].direct, super::McpDirect::None);
        assert_eq!(
            loaded.servers["some"].direct,
            super::McpDirect::Only(vec!["search".to_string(), "get_issue".to_string()])
        );
        assert!(loaded.warn_many_direct);

        // A value that is none of the three names the server and the field.
        write(
            dir.path(),
            "mcp.toml",
            "[servers.fs]\ncommand = \"npx\"\ndirect = \"all\"\n",
        );
        let error = load_mcp(dir.path(), None).unwrap_err();
        assert!(error.contains("mcp.toml"), "{error}");
        assert!(error.contains("server fs"), "{error}");
        assert!(error.contains("direct"), "{error}");

        // The `[settings]` default fills an entry that sets none, and an entry
        // that sets its own `direct` wins over it.
        write(
            dir.path(),
            "mcp.toml",
            concat!(
                "[settings]\n",
                "direct = true\n",
                "\n",
                "[servers.plain]\n",
                "command = \"a\"\n",
                "\n",
                "[servers.quiet]\n",
                "command = \"b\"\n",
                "direct = false\n",
            ),
        );
        let loaded = load_mcp(dir.path(), None).expect("loads");
        assert_eq!(loaded.servers["plain"].direct, super::McpDirect::All);
        assert_eq!(loaded.servers["quiet"].direct, super::McpDirect::None);
        assert!(loaded.warn_many_direct);

        // A profile's setting wins over the global one, key by key.
        write(
            dir.path(),
            "mcp.toml",
            concat!(
                "[settings]\n",
                "direct = true\n",
                "warn_many_direct = true\n",
                "\n",
                "[servers.one]\n",
                "command = \"a\"\n",
            ),
        );
        write(
            dir.path(),
            "profiles/work.mcp.toml",
            concat!(
                "[settings]\n",
                "direct = [\"one_tool\"]\n",
                "warn_many_direct = false\n",
                "\n",
                "[servers.two]\n",
                "command = \"b\"\n",
            ),
        );
        let loaded = load_mcp(dir.path(), Some("work")).expect("loads");
        let only = super::McpDirect::Only(vec!["one_tool".to_string()]);
        assert_eq!(loaded.servers["one"].direct, only);
        assert_eq!(loaded.servers["two"].direct, only);
        assert!(!loaded.warn_many_direct);

        // The old placeholder is now an unknown field, like any other.
        write(
            dir.path(),
            "mcp.toml",
            "[servers.fs]\ncommand = \"npx\"\nexposure = \"codemode\"\n",
        );
        let error = load_mcp(dir.path(), None).unwrap_err();
        assert!(error.contains("mcp.toml"), "{error}");
        assert!(error.contains("exposure"), "{error}");
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "reads the written file back")]
    fn mcp_direct_config_keeps_settings() {
        // A rewrite keeps the `[settings]` table, and an entry that turns the
        // default off keeps its own `direct = false`.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write(
            dir.path(),
            "mcp.toml",
            concat!(
                "[settings]\n",
                "direct = true\n",
                "warn_many_direct = false\n",
                "\n",
                "[servers.a]\n",
                "command = \"a\"\n",
                "\n",
                "[servers.b]\n",
                "command = \"b\"\n",
                "direct = false\n",
            ),
        );
        let loaded = super::load_mcp_file(&path).expect("reads");
        super::save_mcp(&path, &loaded.servers, &loaded.settings).expect("writes");
        let back = super::load_mcp_file(&path).expect("reads back");
        assert_eq!(back.settings, loaded.settings);
        assert!(!back.warn_many_direct);
        assert_eq!(back.servers["a"].direct, super::McpDirect::All);
        assert_eq!(back.servers["b"].direct, super::McpDirect::None);
        let text = std::fs::read_to_string(&path).expect("reads the text");
        assert!(
            !text.contains("[servers.a]\ncommand = \"a\"\ndirect"),
            "{text}"
        );
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn mcp_direct_config_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(
            dir.path(),
            "mcp.toml",
            concat!(
                "[servers.all]\n",
                "command = \"a\"\n",
                "direct = true\n",
                "\n",
                "[servers.plain]\n",
                "command = \"b\"\n",
                "\n",
                "[servers.some]\n",
                "command = \"c\"\n",
                "direct = [\"search\"]\n",
            ),
        );
        let loaded = load_mcp(dir.path(), None).expect("loads");
        let path = dir.path().join("written.toml");
        super::save_mcp(&path, &loaded.servers, &loaded.settings).expect("writes");
        let text = std::fs::read_to_string(&path).expect("reads");
        assert!(text.contains("direct = true"), "{text}");
        // A server with no direct tool writes no `direct` key at all.
        let plain = text
            .split("[servers.plain]")
            .nth(1)
            .expect("the entry")
            .split("[servers.")
            .next()
            .expect("the entry");
        assert!(!plain.contains("direct"), "{text}");
        let back = super::load_mcp_file(&path).expect("reads back");
        assert_eq!(back.servers, loaded.servers);
        assert_eq!(
            back.servers["some"].direct,
            super::McpDirect::Only(vec!["search".to_string()])
        );
    }

    /// The two shapes an editor sends, as JSON.
    const STDIO_SPEC: &str = concat!(
        r#"{"name":"fs","command":"/usr/bin/npx","args":["-y","server"],"#,
        r#""env":[{"name":"TOKEN","value":"hunter2"}]}"#
    );

    const HTTP_SPEC: &str = concat!(
        r#"{"type":"http","name":"docs","url":"https://example.com/mcp","#,
        r#""headers":[{"name":"Authorization","value":"Bearer hunter2"}]}"#
    );

    /// A stdio spec is the ACP entry: a name, an absolute command, its args and
    /// its `{name, value}` pairs, with the defaults an `mcp.toml` entry gets.
    #[test]
    fn mcp_spec_stdio_reads_args_and_env() {
        let (name, server) = super::server_from_spec(STDIO_SPEC).expect("reads");
        assert_eq!(name, "fs");
        assert_eq!(
            server.transport,
            super::McpTransport::Stdio {
                command: "/usr/bin/npx".into(),
                args: ["-y", "server"].map(str::to_string).to_vec(),
                env: [("TOKEN".to_string(), "hunter2".to_string())].to_vec(),
                cwd: None,
            }
        );
        assert_eq!(
            server.timeout,
            std::time::Duration::from_secs(super::DEFAULT_TIMEOUT)
        );
        assert!(server.enabled);
        assert_eq!(server.direct, super::McpDirect::None);
        assert_eq!(server.oauth, None);
    }

    /// An http spec is a URL and its headers, and needs no command.
    #[test]
    fn mcp_spec_http_reads_url_and_headers() {
        let (name, server) = super::server_from_spec(HTTP_SPEC).expect("reads");
        assert_eq!(name, "docs");
        assert_eq!(
            server.transport,
            super::McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: [("Authorization".to_string(), "Bearer hunter2".to_string())].to_vec(),
            }
        );
        assert_eq!(
            server.timeout,
            std::time::Duration::from_secs(super::DEFAULT_TIMEOUT)
        );
        assert!(server.enabled);
        assert_eq!(server.oauth, None);
    }

    /// Each wrong spec is refused with the word that says which part is wrong.
    #[test]
    fn mcp_spec_errors_name_what_is_wrong() {
        for (json, wanted) in [
            ("{", "JSON"),
            (r#"{"command":"/bin/cat"}"#, "name"),
            (r#"{"name":"fs"}"#, "command"),
            (r#"{"type":"http","name":"docs"}"#, "url"),
            (r#"{"name":"fs","command":"npx"}"#, "absolute"),
            (r#"{"name":"fs","command":""}"#, "absolute"),
            (
                r#"{"type":"sse","name":"fs","url":"https://example.com/sse"}"#,
                "sse",
            ),
            (
                r#"{"type":"bogus","name":"fs","command":"/bin/cat"}"#,
                "bogus",
            ),
        ] {
            let error = super::server_from_spec(json).unwrap_err();
            assert!(error.contains(wanted), "{json}: {error}");
        }
        // A name that `mcp.toml` could not hold names the name and the rule.
        let error = super::server_from_spec(r#"{"name":"a b","command":"/bin/cat"}"#).unwrap_err();
        assert!(error.contains("a b"), "{error}");
        assert!(error.contains("lowercase"), "{error}");
    }

    /// Neither the `Debug` of a spec's server nor an error message holds an env
    /// or a header value.
    #[test]
    fn mcp_spec_hides_the_values() {
        let (_, server) = super::server_from_spec(STDIO_SPEC).expect("reads");
        let text = format!("{server:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("TOKEN"), "{text}");
        let (_, server) = super::server_from_spec(HTTP_SPEC).expect("reads");
        let text = format!("{server:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("Authorization"), "{text}");
        // An error that names another part still holds no value.
        let error = super::server_from_spec(
            r#"{"name":"FS","command":"/bin/cat","env":[{"name":"TOKEN","value":"hunter2"}]}"#,
        )
        .unwrap_err();
        assert!(!error.contains("hunter2"), "{error}");
    }
}
