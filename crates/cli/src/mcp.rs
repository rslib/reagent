//! The `mcp` command: add, remove, list, login and logout of MCP servers.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use reagent_config::mcp::{self, DEFAULT_TIMEOUT, valid_server_name};
use reagent_config::{McpOAuth, McpServer, McpTransport, Secret};
use reagent_mcp::auth::ServerAuth;
use reagent_mcp::{ServerStatus, Servers};
use reagent_oauth::loopback::opener;

/// The usage of the command, printed by `mcp` and `mcp --help`.
const USAGE: &str = concat!(
    "Usage:\n",
    "  reagent mcp add <name> [--profile <p>] [--env KEY=VALUE]... [--timeout <s>] [--disabled]",
    "\n                [--direct | --direct-tool <tool>...] -- <command> [args...]\n",
    "  reagent mcp add <name> [--profile <p>] --url <url> [--header 'Name: value']...\n",
    "                [--bearer-token-env-var VAR] [--client-id <id>]\n",
    "                [--client-secret <value> | --client-secret-env VAR]\n",
    "                [--callback-port <n>] [--scope <s>] [--timeout <s>] [--disabled]\n",
    "                [--direct | --direct-tool <tool>...]\n",
    "  reagent mcp remove <name> [--profile <p>]\n",
    "  reagent mcp list [--profile <p>]\n",
    "  reagent mcp login <name> [--profile <p>] [--no-browser]\n",
    "  reagent mcp logout <name>\n",
    "\n",
    "The global file is mcp.toml in the config folder; --profile work writes",
    " profiles/work.mcp.toml.\n",
    "Values are written as given: --env KEY='${OTHER}' stores the text ${OTHER}.\n",
    "A header value may be written as '${VAR}' to keep the secret out of shell",
    " history.\n",
    "--direct offers every tool of the server to the model as a normal tool;",
    " --direct-tool names one, more than once if needed.\n",
);

/// What a run says when `--client-secret` writes a value from the command line.
const CLIENT_SECRET_HISTORY: &str =
    "reagent: --client-secret puts the secret in your shell history; use --client-secret-env VAR";

/// How often a waiting `list` looks at its servers again.
const POLL: Duration = Duration::from_millis(20);

/// How many bytes of the tool names `list` shows.
const TOOLS_LIMIT: usize = 200;

/// Runs one `mcp` command and returns the process exit code.
pub async fn run(args: Vec<OsString>) -> ExitCode {
    let outcome = match args.first().and_then(|arg| arg.to_str()) {
        None | Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some("add") => add(args[1..].to_vec()).await,
        Some("remove") => remove(args[1..].to_vec()).await,
        Some("list") => list(args[1..].to_vec()).await,
        Some("login") => login(args[1..].to_vec()).await,
        Some("logout") => logout(args[1..].to_vec()).await,
        Some(other) => Err(format!("unknown mcp command: {other}")),
    };
    match outcome {
        Ok(code) => code,
        Err(error) => {
            eprintln!("reagent: {error}");
            ExitCode::from(1)
        }
    }
}

/// Runs the blocking file work off the async thread.
async fn config_task<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| "the config task failed".to_string())?
}

/// The global config folder the server files live in.
fn config_dir() -> Result<PathBuf, String> {
    reagent_config::global_dir()
        .ok_or_else(|| "no config folder; set HOME or XDG_CONFIG_HOME".to_string())
}

/// Refuses a name that no `[servers.<name>]` table can carry.
fn check_name(name: &str) -> Result<(), String> {
    if valid_server_name(name) {
        return Ok(());
    }
    Err(format!(
        "invalid server name {name}: use lowercase letters, digits, - and _ only"
    ))
}

/// Refuses a name that no `${VAR}` reference can read.
fn check_variable(name: &str) -> Result<(), String> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Ok(());
    }
    Err(format!(
        "invalid variable name {name}: use letters, digits and _ only"
    ))
}

/// The value after `flag`.
fn value(args: &mut std::slice::Iter<'_, OsString>, flag: &str) -> Result<String, String> {
    args.next()
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| format!("missing value for {flag}"))
}

/// The number after `flag`.
fn number<T: std::str::FromStr>(text: &str, flag: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("invalid value for {flag}"))
}

/// The `add` command: writes one entry and reads it back.
async fn add(args: Vec<OsString>) -> Result<ExitCode, String> {
    let add = Add::parse(&args)?;
    let name = add.name.clone();
    let path = config_task(move || add.write()).await?;
    println!("added {name} to {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Every argument of `mcp add`.
#[derive(Default)]
struct Add {
    /// The name of the entry.
    name: String,
    /// The profile whose file the entry goes in.
    profile: Option<String>,
    /// The environment variables of a stdio server, as written.
    env: Vec<(String, String)>,
    /// The headers of an HTTP server, as written.
    headers: Vec<(String, String)>,
    /// The seconds one call may take.
    timeout: Option<u64>,
    /// Whether a run leaves the server out.
    disabled: bool,
    /// Whether every tool of the server goes into the request directly.
    direct: bool,
    /// The tools that go into the request directly, by their MCP names.
    direct_tools: Vec<String>,
    /// The endpoint of an HTTP server.
    url: Option<String>,
    /// The program of a stdio server.
    command: Option<String>,
    /// The arguments of that program.
    program_args: Vec<String>,
    /// The client id registered by hand.
    client_id: Option<String>,
    /// The secret of that client.
    client_secret: Option<String>,
    /// The port the login callback listens on.
    callback_port: Option<u16>,
    /// The scopes the login asks for.
    scope: Option<String>,
}

impl Add {
    /// Reads the arguments of `mcp add`.
    fn parse(args: &[OsString]) -> Result<Add, String> {
        let mut add = Add::default();
        let mut name = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let text = arg.to_string_lossy();
            match text.as_ref() {
                "--" => {
                    add.command = args.next().map(|word| word.to_string_lossy().into_owned());
                    add.program_args = args
                        .by_ref()
                        .map(|word| word.to_string_lossy().into_owned())
                        .collect();
                    break;
                }
                "-l" | "--local" => {
                    return Err(
                        "reagent keeps MCP servers out of repo files; add it to mcp.toml or a profile"
                            .to_string(),
                    );
                }
                "--profile" => add.profile = Some(value(&mut args, "--profile")?),
                "--env" => add.env.push(pair(&value(&mut args, "--env")?)?),
                "--header" => add.headers.push(header(&value(&mut args, "--header")?)?),
                "--bearer-token-env-var" => {
                    let name = value(&mut args, "--bearer-token-env-var")?;
                    add.headers
                        .push(("Authorization".to_string(), format!("Bearer ${{{name}}}")));
                }
                "--url" => add.url = Some(value(&mut args, "--url")?),
                "--timeout" => {
                    add.timeout = Some(number(&value(&mut args, "--timeout")?, "--timeout")?)
                }
                "--client-id" => add.client_id = Some(value(&mut args, "--client-id")?),
                "--client-secret" => {
                    add.client_secret = Some(value(&mut args, "--client-secret")?);
                    eprintln!("{CLIENT_SECRET_HISTORY}");
                }
                "--client-secret-env" => {
                    let name = value(&mut args, "--client-secret-env")?;
                    check_variable(&name)?;
                    add.client_secret = Some(format!("${{{name}}}"));
                }
                "--callback-port" => {
                    add.callback_port = Some(number(
                        &value(&mut args, "--callback-port")?,
                        "--callback-port",
                    )?)
                }
                "--scope" => add.scope = Some(value(&mut args, "--scope")?),
                "--disabled" => add.disabled = true,
                "--direct" => add.direct = true,
                "--direct-tool" => add.direct_tools.push(value(&mut args, "--direct-tool")?),
                other if other.starts_with('-') => {
                    return Err(format!("unknown argument: {other}"));
                }
                _ if name.is_some() => return Err("add takes one server name".to_string()),
                _ => name = Some(text.into_owned()),
            }
        }
        add.name = name.ok_or_else(|| "add needs a server name".to_string())?;
        check_name(&add.name)?;
        if add.direct && !add.direct_tools.is_empty() {
            return Err(format!(
                "server {}: set --direct or --direct-tool, not both",
                add.name
            ));
        }
        Ok(add)
    }

    /// The entry this command describes, its pairs in the order a table keeps.
    fn server(&self) -> Result<McpServer, String> {
        let name = &self.name;
        let oauth = self.oauth();
        let mut env = self.env.clone();
        env.sort();
        let mut headers = self.headers.clone();
        headers.sort();
        let transport = match (&self.url, &self.command) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "server {name}: set either command or url, not both"
                ));
            }
            (None, None) => return Err(format!("server {name}: set command or url")),
            (Some(url), None) => {
                if !self.env.is_empty() {
                    return Err(format!("server {name}: --env needs a command"));
                }
                McpTransport::Http {
                    url: url.clone(),
                    headers,
                }
            }
            (None, Some(command)) => {
                if !self.headers.is_empty() || oauth.is_some() {
                    return Err(format!("server {name}: headers and oauth need a url"));
                }
                McpTransport::Stdio {
                    command: command.clone(),
                    args: self.program_args.clone(),
                    env,
                    cwd: None,
                }
            }
        };
        Ok(McpServer {
            transport,
            timeout: Duration::from_secs(self.timeout.unwrap_or(DEFAULT_TIMEOUT)),
            enabled: !self.disabled,
            direct: self.direct_value(),
            oauth,
        })
    }

    /// Whether a flag sets the entry's `direct`.
    fn sets_direct(&self) -> bool {
        self.direct || !self.direct_tools.is_empty()
    }

    /// The `direct` the flags set.
    fn direct_value(&self) -> mcp::McpDirect {
        if !self.direct_tools.is_empty() {
            mcp::McpDirect::Only(self.direct_tools.clone())
        } else if self.direct {
            mcp::McpDirect::All
        } else {
            mcp::McpDirect::None
        }
    }

    /// The login settings the flags name, when they name any.
    fn oauth(&self) -> Option<McpOAuth> {
        let set = self.client_id.is_some()
            || self.client_secret.is_some()
            || self.callback_port.is_some()
            || self.scope.is_some();
        set.then(|| McpOAuth {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.as_deref().map(Secret::from),
            client_metadata_url: None,
            callback_port: self.callback_port,
            scope: self.scope.clone(),
        })
    }

    /// Writes the entry into its file, replacing one of the same name.
    fn write(&self) -> Result<PathBuf, String> {
        let dir = config_dir()?;
        let path = mcp::file(&dir, self.profile.as_deref())?;
        let mut server = self.server()?;
        let mut config = mcp::load_mcp_file(&path)?;
        // No `--direct` flag leaves the entry on the file's `[settings]` default.
        if !self.sets_direct() {
            server.direct = config.settings.direct.clone().unwrap_or_default();
        }
        config.servers.insert(self.name.clone(), server.clone());
        mcp::save_mcp(&path, &config.servers, &config.settings)?;
        // The entry must pass the checks a later load applies.
        let written = mcp::load_mcp_file(&path)?;
        if !reads_back(written.servers.get(&self.name), &server) {
            return Err(format!(
                "cannot write {}: the entry does not read back",
                path.display()
            ));
        }
        Ok(path)
    }
}

/// Whether the entry `written` is `server`.
fn reads_back(written: Option<&McpServer>, server: &McpServer) -> bool {
    written.is_some_and(|entry| entry == server)
}

/// A `KEY=VALUE` pair of `--env`.
fn pair(text: &str) -> Result<(String, String), String> {
    let Some((key, value)) = text.split_once('=') else {
        return Err("--env needs KEY=VALUE".to_string());
    };
    if key.is_empty() {
        return Err("--env needs KEY=VALUE".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

/// A `Name: value` pair of `--header`.
fn header(text: &str) -> Result<(String, String), String> {
    let Some((name, value)) = text.split_once(':') else {
        return Err("--header needs 'Name: value'".to_string());
    };
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return Err("--header needs a name with no blanks".to_string());
    }
    Ok((name.to_string(), value.trim().to_string()))
}

/// The `remove` command: drops one entry.
async fn remove(args: Vec<OsString>) -> Result<ExitCode, String> {
    let (name, profile) = named(&args, "remove")?;
    let (name, path) = config_task(move || {
        let path = mcp::file(&config_dir()?, profile.as_deref())?;
        let mut config = mcp::load_mcp_file(&path)?;
        if config.servers.remove(&name).is_none() {
            return Err(format!("no server {name} in {}", path.display()));
        }
        mcp::save_mcp(&path, &config.servers, &config.settings)?;
        Ok((name, path))
    })
    .await?;
    println!("removed {name} from {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// The server name and the `--profile` a command names.
fn named(args: &[OsString], command: &str) -> Result<(String, Option<String>), String> {
    let mut name = None;
    let mut profile = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--profile") => profile = Some(value(&mut args, "--profile")?),
            Some(text) if text.starts_with('-') => {
                return Err(format!("unknown argument: {text}"));
            }
            _ if name.is_some() => return Err(format!("{command} takes one server name")),
            _ => name = Some(arg.to_string_lossy().into_owned()),
        }
    }
    let name = name.ok_or_else(|| format!("{command} needs a server name"))?;
    check_name(&name)?;
    Ok((name, profile))
}

/// The `--profile` value of a command that takes no name.
fn profile_only(args: &[OsString], command: &str) -> Result<Option<String>, String> {
    let mut profile = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--profile") => profile = Some(value(&mut args, "--profile")?),
            Some(text) if text.starts_with('-') => {
                return Err(format!("unknown argument: {text}"));
            }
            _ => return Err(format!("{command} takes no argument")),
        }
    }
    Ok(profile)
}

/// The `list` command: starts every enabled server and reports its state.
async fn list(args: Vec<OsString>) -> Result<ExitCode, String> {
    let profile = profile_only(&args, "list")?;
    let servers = config_task(move || {
        let dir = config_dir()?;
        reagent_config::load_mcp(&dir, profile.as_deref())
    })
    .await?;
    let enabled: Vec<(String, McpServer)> = servers
        .servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .map(|(name, server)| (name.clone(), server.clone()))
        .collect();
    let auth_dir = reagent_config::auth::dir().ok();
    let running = Servers::start(&enabled, auth_dir);
    wait(&running, &servers.servers).await;
    let states: BTreeMap<String, ServerStatus> = running
        .snapshot()
        .into_iter()
        .map(|state| (state.name, state.status))
        .collect();
    let mut not_ready = false;
    for (name, server) in &servers.servers {
        if !server.enabled {
            println!(
                "{name}  disabled  {}{}",
                target(server),
                direct_text(server)
            );
            continue;
        }
        match states.get(name) {
            Some(ServerStatus::Ready(tools)) => {
                println!("{name}  ready  {}{}", target(server), direct_text(server));
                let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
                println!("  {} tools: {}", names.len(), tools_text(&names));
            }
            Some(ServerStatus::Failed(reason)) => {
                not_ready = true;
                println!("{name}  failed  {}{}", target(server), direct_text(server));
                println!("  error: {reason}");
            }
            _ => {
                not_ready = true;
                println!("{name}  failed  {}{}", target(server), direct_text(server));
                println!("  error: no answer in {}s", server.timeout.as_secs());
            }
        }
    }
    running.stop().await;
    Ok(if not_ready {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Waits for every starting server, each up to its own timeout.
async fn wait(running: &Servers, servers: &BTreeMap<String, McpServer>) {
    let began = Instant::now();
    loop {
        let waiting: Vec<Duration> = running
            .snapshot()
            .iter()
            .filter(|state| matches!(state.status, ServerStatus::Starting))
            .map(|state| {
                servers
                    .get(&state.name)
                    .map_or(DEFAULT_TIMEOUT, |server| server.timeout.as_secs())
            })
            .map(Duration::from_secs)
            .collect();
        let taken = began.elapsed();
        let left = waiting
            .iter()
            .filter(|timeout| **timeout > taken)
            .map(|timeout| *timeout - taken)
            .min();
        let Some(left) = left else {
            return;
        };
        tokio::time::sleep(POLL.min(left)).await;
    }
}

/// The stdio command or the scheme, host and port of the url of `server`,
/// which may carry a key in its path or query.
fn target(server: &McpServer) -> String {
    match &server.transport {
        McpTransport::Stdio { command, args, .. } => {
            let words = std::iter::once(command.as_str()).chain(args.iter().map(String::as_str));
            words.collect::<Vec<&str>>().join(" ")
        }
        McpTransport::Http { url, .. } => reagent_oauth::metadata::origin(url).to_string(),
    }
}

/// The tool names of one server, cut short when they are long.
fn tools_text(names: &[&str]) -> String {
    let joined = names.join(", ");
    if joined.len() <= TOOLS_LIMIT {
        return joined;
    }
    format!("{}...", reagent_util::text::head(&joined, TOOLS_LIMIT))
}

/// The `direct` part of a server's first line in `mcp list`.
fn direct_text(server: &McpServer) -> String {
    match &server.direct {
        mcp::McpDirect::None => String::new(),
        mcp::McpDirect::All => "  direct: all".to_string(),
        mcp::McpDirect::Only(names) => format!("  direct: {}", names.join(", ")),
    }
}

/// The `login` command: runs the browser login of one HTTP server.
async fn login(args: Vec<OsString>) -> Result<ExitCode, String> {
    let (name, profile, browser) = login_args(&args)?;
    let servers = config_task(move || {
        let dir = config_dir()?;
        reagent_config::load_mcp(&dir, profile.as_deref())
    })
    .await?;
    let server = servers
        .servers
        .get(&name)
        .ok_or_else(|| format!("no server {name}; add it with reagent mcp add"))?;
    let McpTransport::Http { url, .. } = &server.transport else {
        return Err(format!("{name}: stdio servers take credentials from env"));
    };
    let oauth = server.oauth.clone().unwrap_or_default();
    let auth = ServerAuth {
        name: name.clone(),
        url: url.clone(),
        client_id: oauth.client_id,
        client_secret: oauth.client_secret,
        client_metadata_url: oauth.client_metadata_url,
        callback_port: oauth.callback_port,
        scope: oauth.scope,
    };
    let dir = reagent_config::auth::dir()?;
    reagent_mcp::auth::login(&auth, &dir, |url| {
        eprintln!("Open this URL to log in:\n{url}\nOr paste the URL you are sent to here.");
        if browser {
            opener(url);
        }
    })
    .await
    .map_err(|error| error.to_string())?;
    println!("logged in to {name}");
    Ok(ExitCode::SUCCESS)
}

/// The name, the profile and whether to open a browser of `mcp login`.
fn login_args(args: &[OsString]) -> Result<(String, Option<String>, bool), String> {
    let mut browser = true;
    let mut rest = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--no-browser") => browser = false,
            Some("--profile") => {
                rest.push(arg.clone());
                rest.push(
                    args.next()
                        .ok_or_else(|| "missing value for --profile".to_string())?
                        .clone(),
                );
            }
            Some(text) if text.starts_with('-') => {
                return Err(format!("unknown argument: {text}"));
            }
            _ => rest.push(arg.clone()),
        }
    }
    let (name, profile) = named(&rest, "login")?;
    Ok((name, profile, browser))
}

/// The `logout` command: drops every saved login of one server.
async fn logout(args: Vec<OsString>) -> Result<ExitCode, String> {
    if args.len() != 1 {
        return Err("logout needs one server name".to_string());
    }
    let name = args[0].to_string_lossy().into_owned();
    check_name(&name)?;
    let gone = name.clone();
    let removed = config_task(move || {
        let dir = reagent_config::auth::dir()?;
        reagent_mcp::auth::logout(&gone, &dir).map_err(|error| error.to_string())
    })
    .await?;
    if removed {
        println!("logged out of {name}");
    } else {
        println!("no login for {name}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(url: &str) -> McpServer {
        McpServer {
            transport: McpTransport::Http {
                url: url.to_string(),
                headers: Vec::new(),
            },
            timeout: Duration::from_secs(DEFAULT_TIMEOUT),
            enabled: true,
            direct: mcp::McpDirect::None,
            oauth: None,
        }
    }

    #[test]
    fn list_target_holds_no_path_and_no_query() {
        assert_eq!(
            target(&http("https://example.com:8443/mcp?key=abc")),
            "https://example.com:8443"
        );
        assert!(!target(&http("https://example.com/mcp?key=abc")).contains("abc"));
        assert_eq!(target(&http("https://example.com")), "https://example.com");
    }
}
