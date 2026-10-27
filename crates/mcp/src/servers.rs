//! A set of MCP servers started in the background, and their tools as agent
//! tools.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use reagent_agent::tool::{EffectClass, Object, Tool as AgentTool, ToolOutput};
use reagent_ai::ToolSpec;
use reagent_config::mcp::McpDirect;
use reagent_config::{McpServer, McpTransport};
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthStore;
use crate::client::{Client, StdioSpec, Tool};
use crate::http::{HttpSpec, expand_env};

/// The prefix of every name a server's tool is exposed under.
pub const EXPOSED_PREFIX: &str = "mcp__";

/// The separator between the server and the tool of an exposed name.
const SEPARATOR: &str = "__";

/// How often a waiting call looks at the state again.
const POLL: Duration = Duration::from_millis(10);

/// The generation a started set begins at, so a source that tracks its tool
/// list never reports the untracked one.
const FIRST_GENERATION: u64 = 1;

/// How long stopping a server waits for a call that still holds it.
const STOP_WAIT: Duration = Duration::from_secs(2);

/// The annotation that says a tool only reads state.
const READ_ONLY: &str = "readOnlyHint";

/// The longest tool name a provider accepts.
const MAX_NAME: usize = 64;

/// How many characters of a long name a capped name keeps.
const KEEP: usize = 55;

/// How many hex digits of the name's hash a capped name ends with.
const HASH_DIGITS: usize = 8;

/// Where one server is.
enum Status {
    /// It has not answered yet.
    Starting,
    /// It listed these tools.
    Ready {
        /// The connection its calls go through.
        client: Arc<Client>,
        /// What it offers.
        tools: Arc<[Tool]>,
    },
    /// It failed with this reason.
    Failed(String),
}

/// One server of the set.
struct Entry {
    name: String,
    /// What the exposed names of this server start with.
    prefix: String,
    /// When it started, so a wait is counted from there.
    started: Instant,
    timeout: Duration,
    /// Which of its tools the request offers as tools of their own.
    direct: McpDirect,
    status: Status,
}

/// The state every handle of one set shares.
struct State {
    servers: Vec<Entry>,
    /// The exposed name, and the server and tool it comes from.
    exposed: BTreeMap<String, (usize, String)>,
    warnings: Vec<String>,
    /// Counts the relists that changed a server's tools.
    generation: u64,
}

impl State {
    /// The server of `exposed`: the one that owns the name, else the starting
    /// one whose names it may come under.
    fn owner(&self, exposed: &str) -> Option<usize> {
        if let Some((server, _)) = self.exposed.get(exposed) {
            return Some(*server);
        }
        self.servers
            .iter()
            .position(|entry| exposed.starts_with(&entry.prefix))
    }
}

/// The state every clone shares.
struct Inner {
    state: Mutex<State>,
    /// Bumped on every change of `state`, so a wait wakes on it.
    changed: watch::Sender<u64>,
}

impl Inner {
    /// The state, locked for one short read or write.
    fn state(&self) -> MutexGuard<'_, State> {
        // Every change is one assignment, so a panic elsewhere cannot leave
        // half a server here.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wakes every wait that looks at this state.
    fn tell(&self) {
        self.changed
            .send_modify(|seen| *seen = seen.wrapping_add(1));
    }

    /// Sets the tools of server `index`, replacing the ones it had.
    fn relist(&self, index: usize, client: Arc<Client>, tools: Arc<[Tool]>) {
        relist(&mut self.state(), index, client, tools);
        self.tell();
    }

    /// Takes the new list of server `index`, unless the set stopped or the
    /// list changed again since `old` was read.
    fn refreshed(&self, index: usize, old: &Arc<[Tool]>, client: Arc<Client>, tools: Arc<[Tool]>) {
        let mut state = self.state();
        if matches!(
            &state.servers[index].status,
            Status::Ready { tools: now, .. } if Arc::ptr_eq(now, old)
        ) {
            relist(&mut state, index, client, tools);
            drop(state);
            self.tell();
        }
    }

    /// Notes that server `index` failed with `reason`.
    fn failed(&self, index: usize, reason: String) {
        self.state().servers[index].status = Status::Failed(reason);
        self.tell();
    }
}

/// Sets the tools of server `index` in `state`, replacing the ones it had.
fn relist(state: &mut State, index: usize, client: Arc<Client>, tools: Arc<[Tool]>) {
    let prefix = state.servers[index].prefix.clone();
    state.exposed.retain(|_, (owner, _)| *owner != index);
    let mut warnings = Vec::new();
    for tool in tools.iter() {
        let exposed = exposed_from(&prefix, &tool.name);
        if state.exposed.contains_key(&exposed) {
            warnings.push(format!(
                "two MCP tools are exposed as {exposed}; the first one is used"
            ));
            continue;
        }
        state.exposed.insert(exposed, (index, tool.name.clone()));
    }
    state.warnings.extend(warnings);
    state.servers[index].status = Status::Ready { client, tools };
    state.generation += 1;
}

/// A ready server as a refresh reads it: its index, connection, tools and
/// timeout.
type Listed = (usize, Arc<Client>, Arc<[Tool]>, Duration);

/// A set of MCP servers, each started in the background.
#[derive(Clone)]
pub struct Servers {
    inner: Arc<Inner>,
}

impl fmt::Debug for Servers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Servers")
            .field("servers", &self.snapshot())
            .finish()
    }
}

/// What one server is doing, for the start line and the description.
#[derive(Clone, Debug)]
pub struct ServerState {
    /// The server's name, as `mcp.toml` writes it.
    pub name: String,
    /// Where the server is.
    pub status: ServerStatus,
}

/// Where one server is.
#[derive(Clone, Debug)]
pub enum ServerStatus {
    /// It has not answered yet.
    Starting,
    /// It listed these tools.
    Ready(Arc<[Tool]>),
    /// It failed with this reason.
    Failed(String),
}

impl Servers {
    /// Starts each of `servers` in the background, taking a login from
    /// `auth_dir` where a server has one.
    pub fn start(servers: &[(String, McpServer)], auth_dir: Option<PathBuf>) -> Servers {
        let mut entries = Vec::with_capacity(servers.len());
        let mut specs = Vec::with_capacity(servers.len());
        for (name, server) in servers {
            let built = spec(name, server, auth_dir.as_deref());
            entries.push(Entry {
                name: name.clone(),
                prefix: format!("{EXPOSED_PREFIX}{}{SEPARATOR}", sanitize(name)),
                started: Instant::now(),
                timeout: server.timeout,
                direct: server.direct.clone(),
                status: match &built {
                    Ok(_) => Status::Starting,
                    Err(reason) => Status::Failed(reason.clone()),
                },
            });
            specs.push(built);
        }
        let (changed, _) = watch::channel(FIRST_GENERATION);
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                servers: entries,
                exposed: BTreeMap::new(),
                warnings: Vec::new(),
                generation: FIRST_GENERATION,
            }),
            changed,
        });
        for (index, built) in specs.into_iter().enumerate() {
            let Ok(built) = built else {
                continue;
            };
            let inner = Arc::clone(&inner);
            tokio::spawn(async move {
                match connect(built).await {
                    Ok((client, tools)) => inner.relist(index, Arc::new(client), tools),
                    Err(reason) => inner.failed(index, reason),
                }
            });
        }
        Servers { inner }
    }

    /// Takes the tools of every ready server that changed its list since it
    /// started, and the names they are exposed under.
    pub async fn refresh(&self) {
        let ready: Vec<Listed> = {
            let state = self.inner.state();
            state
                .servers
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| match &entry.status {
                    Status::Ready { client, tools } => {
                        Some((index, Arc::clone(client), Arc::clone(tools), entry.timeout))
                    }
                    Status::Starting | Status::Failed(_) => None,
                })
                .collect()
        };
        // Every list is read at once, so the wait is the slowest server.
        let mut listing = JoinSet::new();
        for (index, client, tools, timeout) in ready {
            // A server that hangs on its list keeps the old one, so a script
            // never waits past the server's own timeout here.
            listing.spawn(async move {
                let list = tokio::time::timeout(timeout, client.tools()).await;
                (index, client, tools, list)
            });
        }
        while let Some(finished) = listing.join_next().await {
            let Ok((index, client, tools, Ok(Ok(list)))) = finished else {
                continue;
            };
            if Arc::ptr_eq(&list, &tools) {
                continue;
            }
            self.inner.refreshed(index, &tools, client, list);
        }
    }

    /// Counts the relists that changed a server's tools.
    pub fn generation(&self) -> u64 {
        self.inner.state().generation
    }

    /// Waits until no server is starting, or `limit` passes.
    pub async fn wait_ready(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut changed = self.inner.changed.subscribe();
        loop {
            let starting = {
                let state = self.inner.state();
                state
                    .servers
                    .iter()
                    .any(|entry| matches!(entry.status, Status::Starting))
            };
            if !starting {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let _ = tokio::time::timeout(deadline - now, changed.changed()).await;
        }
    }

    /// The adapter of the tool exposed as `exposed`, waiting for a starting
    /// server up to its own timeout.
    pub async fn tool(&self, exposed: &str) -> Result<McpTool, String> {
        let mut changed = self.inner.changed.subscribe();
        loop {
            let (index, deadline) = {
                let state = self.inner.state();
                let Some(index) = state.owner(exposed) else {
                    return Err(format!("unknown tool `{exposed}`"));
                };
                let entry = &state.servers[index];
                match &entry.status {
                    Status::Ready { client, tools } => {
                        return adapter(&state, exposed, client, tools);
                    }
                    Status::Failed(reason) => {
                        return Err(format!("the MCP server `{}` failed: {reason}", entry.name));
                    }
                    Status::Starting => {}
                }
                (index, entry.started + entry.timeout)
            };
            let now = Instant::now();
            if now >= deadline {
                let state = self.inner.state();
                let entry = &state.servers[index];
                if let Status::Failed(reason) = &entry.status {
                    return Err(format!("the MCP server `{}` failed: {reason}", entry.name));
                }
                return Err(format!(
                    "the MCP server `{}` did not answer in time for `{exposed}`",
                    entry.name
                ));
            }
            let _ = tokio::time::timeout(deadline - now, changed.changed()).await;
        }
    }

    /// What every server is doing, in start order.
    pub fn snapshot(&self) -> Vec<ServerState> {
        let state = self.inner.state();
        state
            .servers
            .iter()
            .map(|entry| ServerState {
                name: entry.name.clone(),
                status: match &entry.status {
                    Status::Starting => ServerStatus::Starting,
                    Status::Ready { tools, .. } => ServerStatus::Ready(Arc::clone(tools)),
                    Status::Failed(reason) => ServerStatus::Failed(reason.clone()),
                },
            })
            .collect()
    }

    /// The adapters of the tools the config marks direct, in server and tool
    /// order, and a warning for each name a server does not have.
    pub fn direct_adapters(&self) -> Vec<McpTool> {
        let mut state = self.inner.state();
        let mut adapters = Vec::new();
        let mut missing = Vec::new();
        for index in 0..state.servers.len() {
            let direct = state.servers[index].direct.clone();
            if direct == McpDirect::None {
                continue;
            }
            let Status::Ready { client, tools } = &state.servers[index].status else {
                continue;
            };
            let client = Arc::clone(client);
            let tools = Arc::clone(tools);
            match direct {
                McpDirect::None => {}
                McpDirect::All => {
                    for (exposed, (owner, _)) in &state.exposed {
                        if *owner == index
                            && let Ok(adapter) = adapter(&state, exposed, &client, &tools)
                        {
                            adapters.push(adapter);
                        }
                    }
                }
                McpDirect::Only(names) => {
                    for name in names {
                        let exposed = state
                            .exposed
                            .iter()
                            .find(|(_, (owner, tool))| *owner == index && tool == &name)
                            .map(|(exposed, _)| exposed.clone());
                        match exposed {
                            Some(exposed) => {
                                if let Ok(adapter) = adapter(&state, &exposed, &client, &tools) {
                                    adapters.push(adapter);
                                }
                            }
                            None => missing.push(format!(
                                "mcp {}: no tool {name} for direct",
                                state.servers[index].name
                            )),
                        }
                    }
                }
            }
        }
        for warning in missing {
            if !state.warnings.contains(&warning) {
                state.warnings.push(warning);
            }
        }
        adapters
    }

    /// The adapter of every ready tool, in server and tool order.
    pub fn adapters(&self) -> Vec<McpTool> {
        let state = self.inner.state();
        let mut adapters: Vec<McpTool> = Vec::with_capacity(state.exposed.len());
        for (exposed, (index, _)) in &state.exposed {
            let Status::Ready { client, tools } = &state.servers[*index].status else {
                continue;
            };
            if let Ok(adapter) = adapter(&state, exposed, client, tools) {
                adapters.push(adapter);
            }
        }
        adapters
    }

    /// The exposed names of the ready tools, sorted.
    pub fn names(&self) -> Vec<String> {
        let state = self.inner.state();
        state.exposed.keys().cloned().collect()
    }

    /// The name collisions found while starting.
    pub fn warnings(&self) -> Vec<String> {
        self.inner.state().warnings.clone()
    }

    /// Ends every server that answered.
    pub async fn stop(self) {
        let clients = {
            let mut state = self.inner.state();
            let mut clients = Vec::new();
            for entry in &mut state.servers {
                if let Status::Ready { client, .. } =
                    std::mem::replace(&mut entry.status, Status::Starting)
                {
                    clients.push(client);
                }
            }
            state.exposed.clear();
            clients
        };
        self.inner.tell();
        for client in clients {
            stop_client(client).await;
        }
    }
}

/// Ends one client, waiting a moment for a call that still holds it.
async fn stop_client(client: Arc<Client>) {
    let deadline = Instant::now() + STOP_WAIT;
    let mut client = client;
    loop {
        match Arc::try_unwrap(client) {
            Ok(client) => return client.stop().await,
            Err(shared) => {
                if Instant::now() >= deadline {
                    return;
                }
                client = shared;
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

/// The adapter of the tool `exposed` of a ready server.
fn adapter(
    state: &State,
    exposed: &str,
    client: &Arc<Client>,
    tools: &Arc<[Tool]>,
) -> Result<McpTool, String> {
    let Some((index, name)) = state.exposed.get(exposed) else {
        return Err(format!("unknown tool `{exposed}`"));
    };
    let Some(tool) = tools.iter().find(|tool| &tool.name == name) else {
        return Err(format!("unknown tool `{exposed}`"));
    };
    let entry = &state.servers[*index];
    Ok(McpTool::new(
        exposed,
        tool,
        Arc::clone(client),
        &entry.name,
        entry.timeout,
    ))
}

/// The name a server's `tool` is exposed under.
pub fn exposed_name(server: &str, tool: &str) -> String {
    exposed_from(
        &format!("{EXPOSED_PREFIX}{}{SEPARATOR}", sanitize(server)),
        tool,
    )
}

/// The exposed name of `tool` under the prefix of one server, cut to the
/// length a provider takes.
fn exposed_from(prefix: &str, tool: &str) -> String {
    let name = format!("{prefix}{}", sanitize(tool));
    if name.len() <= MAX_NAME {
        return name;
    }
    let kept: String = name.chars().take(KEEP).collect();
    format!(
        "{kept}_{:0width$x}",
        hash(name.as_bytes()),
        width = HASH_DIGITS
    )
}

/// The 32-bit FNV-1a hash of `bytes`, so a long name keeps a stable tail.
fn hash(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// `text` with every character outside `[A-Za-z0-9_]` written as `_`.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The transport spec one server is started from.
enum Built {
    Stdio(StdioSpec),
    Http(HttpSpec),
}

/// The spec of `server`, or the reason it cannot be built.
fn spec(name: &str, server: &McpServer, auth_dir: Option<&Path>) -> Result<Built, String> {
    // A missing variable fails the server by its name, never by a value:
    // values hold secrets.
    let expand = |pairs: &[(String, String)]| -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            let value = expand_env(value, |variable| std::env::var(variable).ok())
                .map_err(|error| format!("server {name}: {error}"))?;
            out.push((key.clone(), value));
        }
        Ok(out)
    };
    Ok(match &server.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => Built::Stdio(StdioSpec {
            command: command.clone(),
            args: args.clone(),
            env: expand(env)?,
            cwd: cwd.clone(),
        }),
        McpTransport::Http { url, headers } => Built::Http(HttpSpec {
            url: url.clone(),
            headers: expand(headers)?,
            auth: server.oauth.as_ref().and_then(|_| {
                auth_dir.map(|dir| AuthStore {
                    name: name.to_string(),
                    dir: dir.to_path_buf(),
                })
            }),
        }),
    })
}

/// Connects to one server and lists its tools.
async fn connect(built: Built) -> Result<(Client, Arc<[Tool]>), String> {
    let client = match &built {
        Built::Stdio(spec) => Client::stdio(spec).await,
        Built::Http(spec) => Client::http(spec).await,
    }
    .map_err(|error| error.to_string())?;
    match client.tools().await {
        Ok(tools) => Ok((client, tools)),
        Err(error) => {
            let reason = error.to_string();
            client.stop().await;
            Err(reason)
        }
    }
}

/// One MCP tool, as the agent's tool interface sees it.
pub struct McpTool {
    spec: ToolSpec,
    /// The name the server knows the tool by.
    name: String,
    /// The name of the server, for a call that runs past its timeout.
    server: String,
    /// How long one call may take, as the server's entry says.
    timeout: Duration,
    /// The connection, which the set of servers owns.
    client: Weak<Client>,
    read_only: bool,
    returns: &'static str,
    output_schema: Option<Value>,
}

impl McpTool {
    /// The adapter of `tool`, which the server `server` exposes as `exposed`.
    fn new(
        exposed: &str,
        tool: &Tool,
        client: Arc<Client>,
        server: &str,
        timeout: Duration,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: exposed.to_owned(),
                description: tool.description.clone().unwrap_or_default(),
                input_schema: tool.input_schema.clone(),
            },
            name: tool.name.clone(),
            server: server.to_owned(),
            timeout,
            client: Arc::downgrade(&client),
            read_only: tool
                .annotations
                .as_ref()
                .and_then(|hints| hints.get(READ_ONLY))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            returns: if tool.output_schema.is_some() {
                "object"
            } else {
                "string"
            },
            output_schema: tool.output_schema.clone(),
        }
    }
}

impl fmt::Debug for McpTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpTool")
            .field("name", &self.spec.name)
            .finish_non_exhaustive()
    }
}

impl AgentTool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn effect(&self) -> EffectClass {
        // An annotation may only make a class stricter, and no server is
        // trusted yet, so every MCP tool counts as reaching the outside world.
        EffectClass::External
    }

    fn returns(&self) -> &'static str {
        self.returns
    }

    fn output_schema(&self) -> Option<&Value> {
        self.output_schema.as_ref()
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(client) = self.client.upgrade() else {
                return ToolOutput::error("the MCP server is not running");
            };
            let output =
                match tokio::time::timeout(self.timeout, client.call(&self.name, args, cancel))
                    .await
                {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => return ToolOutput::error(error.to_string()),
                    Err(_) => {
                        return ToolOutput::error(format!(
                            "the MCP server {} did not answer in {} s",
                            self.server,
                            self.timeout.as_secs()
                        ));
                    }
                };
            if output.is_error {
                return ToolOutput::error(output.text);
            }
            match output.structured {
                Some(Value::Object(fields)) => {
                    ToolOutput::ok(output.text).with_object(Object::new(fields))
                }
                _ => ToolOutput::ok(output.text),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_effect_class_is_external() {
        let tool = McpTool {
            spec: ToolSpec {
                name: "mcp__server__tool".into(),
                description: String::new(),
                input_schema: Value::Null,
            },
            name: "tool".into(),
            server: "server".into(),
            timeout: Duration::from_secs(1),
            client: Weak::new(),
            read_only: true,
            returns: "string",
            output_schema: None,
        };
        assert!(AgentTool::read_only(&tool), "a read-only annotation");
        assert_eq!(AgentTool::effect(&tool), EffectClass::External);
    }
}
