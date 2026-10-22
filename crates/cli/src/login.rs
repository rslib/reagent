//! The `login` and `logout` commands: a saved key per provider, and the
//! OpenRouter and ChatGPT browser logins.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use reagent_ai::oauth;
use reagent_config::Secret;
use reagent_config::auth::{self, Login, Logins};

/// The providers a browser login can open.
const OPENROUTER: &str = "openrouter";

/// The provider OpenAI's "Sign in with ChatGPT" is saved under.
const CHATGPT: &str = "chatgpt";

/// Every provider `login` names.
const PROVIDERS: &str = "openrouter, chatgpt";

/// The path of the ChatGPT callback, part of its redirect URI.
const CHATGPT_PATH: &str = "/auth/callback";

/// How long a login waits for the browser or a pasted URL.
const TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long one callback connection may take to send its request. A browser
/// opens idle connections in advance, so a silent one must not hold the port.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the key exchange may take; the shared client sets no timeout.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// The most of a callback request the login reads.
const REQUEST_LIMIT: usize = 8 * 1024;

/// The endpoints one ChatGPT login talks to.
struct Endpoints {
    /// The token endpoint of the exchange and the refresh.
    token: String,
    /// The account signing keys of the ID token check.
    jwks: String,
    /// The endpoint a logout revokes the tokens at.
    revoke: String,
}

/// The real ChatGPT endpoints the command uses.
fn chatgpt_endpoints() -> Endpoints {
    Endpoints {
        token: oauth::chatgpt::TOKEN.to_string(),
        jwks: oauth::chatgpt::JWKS.to_string(),
        revoke: oauth::chatgpt::REVOKE.to_string(),
    }
}

/// The seconds since the epoch, or 0 before 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Runs `login` or `logout` and returns the process exit code.
pub async fn run(command: &str, args: Vec<OsString>) -> ExitCode {
    let outcome = match command {
        "logout" => logout(&args).await,
        _ => login(&args).await,
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reagent: {error}");
            ExitCode::from(1)
        }
    }
}

/// Runs the blocking auth file work off the async thread.
async fn auth_task<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| "the auth file task failed".to_string())?
}

/// The provider `login` names, whether to open a browser, and the account name
/// `--as` saves the login under.
fn login_args(args: &[OsString]) -> Result<(String, bool, Option<String>), String> {
    let mut name = None;
    let mut browser = true;
    let mut account = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--no-browser") => browser = false,
            Some("--as") => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --as".to_string())?
                    .to_string_lossy()
                    .into_owned();
                crate::args::check_account_name(&value)?;
                account = Some(value);
            }
            Some(text) if text.starts_with('-') => {
                return Err(format!("unknown argument: {text}"));
            }
            _ if name.is_some() => {
                return Err("login takes one provider name".to_string());
            }
            _ => name = Some(arg.to_string_lossy().into_owned()),
        }
    }
    match name {
        Some(name) => Ok((name, browser, account)),
        None => Err(format!(
            "login needs a provider; login supports: {PROVIDERS}"
        )),
    }
}

/// The `login <provider> [--no-browser]` command.
async fn login(args: &[OsString]) -> Result<(), String> {
    let (name, browser, as_name) = login_args(args)?;
    // `--as` names the account the login is saved under; the provider name is
    // the default, so a second subscription is a table of its own.
    let account = as_name.unwrap_or_else(|| name.clone());
    let dir = reagent_config::auth::dir()?;
    match name.as_str() {
        OPENROUTER => {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|error| format!("cannot listen on a local port: {error}"))?;
            let path = format!("/oauth/callback/{}", oauth::random_token()?);
            openrouter(
                &account,
                &dir,
                oauth::openrouter::KEYS,
                &listener,
                &path,
                browser,
                EXCHANGE_TIMEOUT,
                tokio::io::BufReader::new(tokio::io::stdin()),
            )
            .await
        }
        CHATGPT => {
            // The port is fixed because it is part of the redirect URI the
            // provider is told about. A busy port costs the callback, not the
            // login: the URL is printed either way and a paste still works.
            let listener = match TcpListener::bind(("127.0.0.1", oauth::chatgpt::PORT)).await {
                Ok(listener) => Some(listener),
                Err(error) => {
                    eprintln!(
                        "reagent: cannot listen on port {} ({error}); paste the URL you are sent to instead",
                        oauth::chatgpt::PORT
                    );
                    None
                }
            };
            let flow = Flow {
                name: account,
                dir,
                endpoints: chatgpt_endpoints(),
                port: oauth::chatgpt::PORT,
                listener,
                browser,
                exchange_timeout: EXCHANGE_TIMEOUT,
            };
            chatgpt_login(
                &flow,
                tokio::io::BufReader::new(tokio::io::stdin()),
                announce,
            )
            .await
        }
        other => Err(format!("login supports: {PROVIDERS}; there is no {other}")),
    }
}

/// Prints the URL to open, and the way to paste it instead.
fn announce(url: &str) {
    eprintln!("Open this URL to log in:\n{url}\nOr paste the URL you are sent to here.");
}

/// The `logout <provider>` command.
async fn logout(args: &[OsString]) -> Result<(), String> {
    if args.len() != 1 {
        return Err("logout needs one provider name".to_string());
    }
    let name = args[0].to_string_lossy().into_owned();
    let dir = reagent_config::auth::dir()?;
    let dropped = logout_with(&dir, &name, &chatgpt_endpoints()).await?;
    if dropped {
        eprintln!("reagent: logged out of {name}");
    } else {
        eprintln!("reagent: no saved login for {name}");
    }
    Ok(())
}

/// Drops the login of `name` in `dir`, revoking the tokens of a ChatGPT login
/// first.
///
/// A refused revoke prints a warning and the login goes anyway: the saved
/// tokens are on this disk, so keeping them is the worse choice.
async fn logout_with(dir: &Path, name: &str, endpoints: &Endpoints) -> Result<bool, String> {
    let saved = {
        let (dir, name) = (dir.to_path_buf(), name.to_string());
        auth_task(move || read_login(&dir, &name)).await?
    };
    let dropped = {
        let (dir, name) = (dir.to_path_buf(), name.to_string());
        auth_task(move || logout_in(&dir, &name)).await?
    };
    if let (
        CHATGPT,
        true,
        Some(Login::Oauth {
            client_id, refresh, ..
        }),
    ) = (name, dropped, &saved)
        && let Err(error) = revoke(endpoints, client_id, refresh.expose(), REVOKE_TIMEOUT).await
    {
        eprintln!("reagent: {error}; the saved login is removed anyway");
    }
    Ok(dropped)
}

/// How long the revoke may take before the logout gives up on it.
const REVOKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Drops the tokens of `refresh` at `endpoints`, or names the timeout: a
/// stalled revoke endpoint must not hold the logout.
async fn revoke(
    endpoints: &Endpoints,
    client_id: &str,
    refresh: &str,
    timeout: Duration,
) -> Result<(), String> {
    tokio::time::timeout(
        timeout,
        oauth::chatgpt::revoke(&endpoints.revoke, client_id, refresh),
    )
    .await
    .map_err(|_| "chatgpt logout failed: the revoke timed out".to_string())?
}

/// The saved login of `name` in `dir`, without dropping it.
fn read_login(dir: &Path, name: &str) -> Result<Option<Login>, String> {
    Ok(auth::read(dir)?.get(name).cloned())
}

/// Drops the login of `name` from `dir`; true when one was saved.
fn logout_in(dir: &Path, name: &str) -> Result<bool, String> {
    let mut logins = auth::read(dir)?;
    if !logins.remove(name) {
        return Ok(false);
    }
    auth::write(dir, &logins)?;
    Ok(true)
}

/// Saves `key` as the OpenRouter login in `logins`, which the login read
/// before the exchange, and writes them to `dir`.
async fn save(dir: &Path, mut logins: Logins, key: &str, name: &str) -> Result<(), String> {
    let login = Login::Key {
        key: Secret::from(key),
    };
    let dir = dir.to_path_buf();
    let name = name.to_string();
    auth_task(move || {
        logins.set(&name, login);
        auth::write(&dir, &logins)
    })
    .await
}

/// One OpenRouter login, saved under `dir`.
///
/// The listener, `keys_url` and `input` are parameters so a test drives the
/// flow against its own port, a fake server and a pasted URL; the command
/// passes a fresh local listener, the real endpoint and stdin.
#[expect(
    clippy::too_many_arguments,
    reason = "one login flow: the account, the folder, the endpoint, the callback, the browser and the input"
)]
async fn openrouter(
    name: &str,
    dir: &Path,
    keys_url: &str,
    listener: &TcpListener,
    path: &str,
    browser: bool,
    exchange_timeout: Duration,
    input: impl AsyncBufRead + Unpin + Send + 'static,
) -> Result<(), String> {
    let pkce = oauth::pkce()?;
    // The auth file is read before the URL is printed: a refused or broken
    // file fails the login while no code has been issued yet.
    let logins = {
        let dir = dir.to_path_buf();
        auth_task(move || auth::read(&dir)).await?
    };
    let port = listener
        .local_addr()
        .map_err(|error| format!("cannot read the local port: {error}"))?
        .port();
    let callback = format!("http://127.0.0.1:{port}{path}");
    let url = oauth::openrouter::authorize_url(&callback, &pkce.challenge);
    announce(&url);
    if browser {
        opener(&url);
    }
    let code = tokio::time::timeout(TIMEOUT, wait_for_code(Some(listener), path, input))
        .await
        .map_err(|_| "login timed out".to_string())??;
    let key = tokio::time::timeout(
        exchange_timeout,
        oauth::openrouter::exchange(keys_url, &code, &pkce.verifier),
    )
    .await
    .map_err(|_| "openrouter login timed out".to_string())??;
    save(dir, logins, &key, name).await?;
    eprintln!("reagent: logged in to {name}");
    Ok(())
}

/// What one ChatGPT login reads and where it answers.
struct Flow {
    /// The name the login is saved under: the provider, or `--as`.
    name: String,
    /// The state folder the logins are saved in.
    dir: PathBuf,
    /// The endpoints the login talks to.
    endpoints: Endpoints,
    /// The local port the redirect URI names.
    port: u16,
    /// The local callback listener, when the port could be bound.
    listener: Option<TcpListener>,
    /// Whether to open a browser.
    browser: bool,
    /// How long the exchange and the key fetch may take.
    exchange_timeout: Duration,
}

/// One "Sign in with ChatGPT" login, saved under `flow.dir`.
///
/// The endpoints, the listener and `input` are parameters so a test drives the
/// flow against its own servers and a pasted URL; the command passes the real
/// endpoints, port 1455 and stdin.
async fn chatgpt_login(
    flow: &Flow,
    input: impl AsyncBufRead + Unpin + Send + 'static,
    announce: impl Fn(&str),
) -> Result<(), String> {
    let pkce = oauth::pkce()?;
    let state = oauth::random_token()?;
    let nonce = oauth::random_token()?;
    let mut logins = {
        let dir = flow.dir.clone();
        auth_task(move || auth::read(&dir)).await?
    };
    // A later login is the same agent host, with the client id and the ID
    // token of the first one.
    let prior = match logins.get(&flow.name) {
        Some(Login::Oauth {
            client_id,
            host_id,
            id_token,
            ..
        }) => Some((
            client_id.clone(),
            host_id.clone(),
            id_token.expose().to_string(),
        )),
        _ => None,
    };
    let (client_id, host_id, hint) = match &prior {
        Some((client_id, host_id, id_token)) => {
            (client_id.clone(), host_id.clone(), Some(id_token.clone()))
        }
        None => (
            oauth::chatgpt::DYNAMIC_CLIENT_ID.to_string(),
            oauth::host_id()?,
            None,
        ),
    };
    let redirect = format!("http://127.0.0.1:{}{CHATGPT_PATH}", flow.port);
    let url = oauth::chatgpt::authorize_url(&oauth::chatgpt::AuthorizeParams {
        client_id: &client_id,
        redirect_uri: &redirect,
        state: &state,
        nonce: &nonce,
        challenge: &pkce.challenge,
        host_id: &host_id,
        first: prior.is_none(),
        id_token_hint: hint.as_deref(),
    });
    announce(&url);
    if flow.browser {
        opener(&url);
    }
    let query = tokio::time::timeout(
        TIMEOUT,
        wait_for_query(
            flow.listener.as_ref(),
            CHATGPT_PATH,
            true,
            Some(&state),
            input,
        ),
    )
    .await
    .map_err(|_| "login timed out".to_string())??;
    // The state is checked before anything the callback carries is used.
    if field(&query, "state") != Some(state.as_str()) {
        return Err("chatgpt login failed: the state does not match".to_string());
    }
    if let Some(error) = field(&query, "error") {
        return Err(match error {
            "access_denied" => "login was denied".to_string(),
            other => format!("login failed: {other}"),
        });
    }
    let issued = match &prior {
        Some((client_id, _, _)) => {
            if let Some(got) = field(&query, "client_id")
                && got != client_id
            {
                return Err("chatgpt login failed: the callback names another client".to_string());
            }
            client_id.clone()
        }
        None => match field(&query, "client_id") {
            Some(got) if got != oauth::chatgpt::DYNAMIC_CLIENT_ID => got.to_string(),
            _ => {
                return Err(
                    "chatgpt login failed: the callback names no issued client id".to_string(),
                );
            }
        },
    };
    let code = field(&query, "code").ok_or_else(|| "the login callback had no code".to_string())?;
    let reply = tokio::time::timeout(
        flow.exchange_timeout,
        oauth::chatgpt::exchange(
            &flow.endpoints.token,
            &issued,
            code,
            &pkce.verifier,
            &redirect,
        ),
    )
    .await
    .map_err(|_| "chatgpt login timed out".to_string())??;
    let id_token = reply
        .id_token
        .clone()
        .ok_or_else(|| "chatgpt login failed: the token reply had no id token".to_string())?;
    let jwks = tokio::time::timeout(
        flow.exchange_timeout,
        oauth::chatgpt::jwks(&flow.endpoints.jwks),
    )
    .await
    .map_err(|_| "chatgpt login timed out".to_string())??;
    let claims = oauth::jwt::verify_id_token(
        id_token.expose(),
        &jwks,
        oauth::chatgpt::ISSUER,
        &issued,
        &nonce,
        now(),
    )
    .map_err(|error| format!("chatgpt login failed: {error}"))?;
    let scope = reply.scope.as_deref().unwrap_or_default();
    if !scope
        .split(' ')
        .any(|one| one == oauth::chatgpt::PLAN_SCOPE)
    {
        return Err("this ChatGPT account did not grant plan use".to_string());
    }
    let login = Login::Oauth {
        client_id: issued,
        host_id,
        access: reply.access_token.clone(),
        refresh: reply.refresh_token.clone(),
        id_token,
        expires_at: now().saturating_add(reply.expires_in),
        subject: claims.sub,
        email: claims.email,
    };
    logins.set(&flow.name, login);
    let dir = flow.dir.clone();
    auth_task(move || auth::write(&dir, &logins)).await?;
    eprintln!("reagent: logged in to {}", flow.name);
    if prior.is_none() {
        eprintln!(
            "reagent: this uses your ChatGPT plan; manage usage at https://chatgpt.com/settings/usage"
        );
    }
    Ok(())
}

/// Opens `url` in the browser, ignoring every failure: the URL is printed, so
/// a paste still works.
#[expect(
    clippy::disallowed_types,
    reason = "the process feature of tokio is not built in; the opener is a fire-and-forget spawn"
)]
fn opener(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The `code` of the callback request, or of the pasted URL, whichever is
/// first.
async fn wait_for_code(
    listener: Option<&TcpListener>,
    path: &str,
    input: impl AsyncBufRead + Unpin + Send + 'static,
) -> Result<String, String> {
    let query = wait_for_query(listener, path, false, None, input).await?;
    field(&query, "code")
        .map(str::to_string)
        .ok_or_else(|| "the login callback had no code".to_string())
}

/// The query of the callback request, or of the pasted URL, whichever is
/// first.
///
/// `keep_errors` accepts a callback that carries `error` instead of `/code`,
/// which is how a ChatGPT login is denied. `state`, when given, is the value
/// the callback must echo: a request that names another one is answered `400`
/// and the wait goes on, so a page on this machine cannot end the login with a
/// code of its own. A listener of `None` waits on the paste alone: the port
/// could not be bound.
async fn wait_for_query(
    listener: Option<&TcpListener>,
    path: &str,
    keep_errors: bool,
    state: Option<&str>,
    input: impl AsyncBufRead + Unpin + Send + 'static,
) -> Result<Vec<(String, String)>, String> {
    let (sender, mut queries) = mpsc::unbounded_channel();
    let mut lines = Some(line_channel(input));
    loop {
        tokio::select! {
            socket = accepted(listener) => {
                let mut socket = socket?;
                // Each connection is read in its own task with its own
                // timeout, so an idle preconnect or a half-sent request of
                // the browser cannot hold up the next connection or a paste.
                let sender = sender.clone();
                let path = path.to_string();
                let state = state.map(str::to_string);
                tokio::spawn(async move {
                    let query =
                        callback(&mut socket, &path, keep_errors, state.as_deref()).await;
                    if let Some(query) = query {
                        let _ = sender.send(query);
                    }
                });
            }
            Some(query) = queries.recv() => return Ok(query),
            line = pasted(&mut lines) => match line {
                Some(line) => {
                    // A paste is the user's own input, so a mismatched state
                    // is reported at once by the caller's check instead of
                    // being ignored as a callback request is.
                    if let Some(query) = oauth::callback_query(line.trim(), path)
                        && carries(&query, keep_errors)
                    {
                        return Ok(query);
                    }
                }
                None => lines = None,
            },
        }
    }
}

/// The next callback connection, or never when there is no listener.
async fn accepted(listener: Option<&TcpListener>) -> Result<TcpStream, String> {
    match listener {
        Some(listener) => listener
            .accept()
            .await
            .map(|(socket, _)| socket)
            .map_err(|error| format!("cannot accept the login callback: {error}")),
        None => std::future::pending().await,
    }
}

/// Whether `query` carries what the login waits for.
fn carries(query: &[(String, String)], keep_errors: bool) -> bool {
    field(query, "code").is_some() || (keep_errors && field(query, "error").is_some())
}

/// The non-empty value of `name` in `query`.
fn field<'a>(query: &'a [(String, String)], name: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

/// The next pasted line; never finishes once the input is closed.
async fn pasted(lines: &mut Option<mpsc::UnboundedReceiver<String>>) -> Option<String> {
    match lines {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Reads the lines of `input` in a task, so a select never loses half a line.
fn line_channel(
    input: impl AsyncBufRead + Unpin + Send + 'static,
) -> mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = input.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Whether `query` echoes the `state` the login sent.
///
/// A page on this machine can reach the callback port, so a request that names
/// another state is refused there and the login keeps waiting for the real
/// one.
fn echoes_state(query: &[(String, String)], state: Option<&str>) -> bool {
    match state {
        Some(state) => field(query, "state") == Some(state),
        None => true,
    }
}

/// Reads one callback request and answers it with the query it carried.
async fn callback(
    socket: &mut TcpStream,
    path: &str,
    keep_errors: bool,
    state: Option<&str>,
) -> Option<Vec<(String, String)>> {
    let Ok(target) = tokio::time::timeout(READ_TIMEOUT, read_target(socket)).await else {
        // A preconnect that sends nothing: leave the port for the next one.
        return None;
    };
    match oauth::callback_query(&target.ok()?, path) {
        Some(query) if carries(&query, keep_errors) && echoes_state(&query, state) => {
            reply(
                socket,
                "200 OK",
                "reagent: login done, you can close this tab.",
            )
            .await;
            Some(query)
        }
        _ => {
            reply(socket, "400 Bad Request", "reagent: no login code there.").await;
            None
        }
    }
}

/// The request target of one request off `socket`.
async fn read_target(socket: &mut TcpStream) -> Result<String, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() >= REQUEST_LIMIT {
            break;
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let request_line = text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    parts.next();
    Ok(parts.next().unwrap_or_default().to_string())
}

/// Answers one callback request and closes it.
async fn reply(socket: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    /// Serves one request with `status` and `reply`; returns the URL and the
    /// recorded body.
    async fn keys_server(status: u16, reply: &'static str) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let address = listener.local_addr().expect("the bound address");
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept a connection");
            let body = read_body(&mut socket).await;
            let _ = sender.send(body);
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{address}"), receiver)
    }

    /// Reads one request off `socket` and returns its body.
    async fn read_body(socket: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                let length: usize = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse().ok())
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    break;
                }
            }
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let text = String::from_utf8_lossy(&buffer).into_owned();
        text.split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default()
    }

    fn saved(dir: &Path, name: &str) -> Option<String> {
        let logins = auth::read(dir).expect("reads the auth file");
        logins
            .get(name)
            .and_then(|login| login.key())
            .map(|key| key.expose().to_string())
    }

    #[cfg(unix)]
    #[expect(clippy::disallowed_methods, reason = "a test reads its own file")]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).expect("stats").permissions().mode() & 0o777
    }

    /// Connects once the flow has bound its port.
    async fn connect(port: u16) -> TcpStream {
        for _ in 0..100 {
            if let Ok(socket) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                return socket;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the login never bound its port");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn login_flow_saves_the_pasted_key_with_mode_600() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (base, recorded) = keys_server(200, r#"{"key":"sk-or-test"}"#).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let pasted: &'static [u8] = b"http://127.0.0.1:4999/oauth/callback/test?code=the-code\n";
        openrouter(
            OPENROUTER,
            dir.path(),
            &format!("{base}/api/v1/auth/keys"),
            &listener,
            "/oauth/callback/test",
            false,
            EXCHANGE_TIMEOUT,
            pasted,
        )
        .await
        .expect("the login works");
        assert_eq!(
            saved(dir.path(), "openrouter").as_deref(),
            Some("sk-or-test")
        );
        assert_eq!(mode(&auth::path(dir.path())), 0o600);
        let body = recorded.await.expect("the server recorded the request");
        assert!(body.contains(r#""code":"the-code""#), "{body}");
        assert!(body.contains("S256"), "{body}");
    }

    #[tokio::test]
    async fn login_flow_reports_a_refused_exchange_without_the_code() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (base, _recorded) = keys_server(401, "{}").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let pasted: &'static [u8] = b"http://127.0.0.1:4999/oauth/callback/test?code=the-code\n";
        let error = openrouter(
            OPENROUTER,
            dir.path(),
            &format!("{base}/api/v1/auth/keys"),
            &listener,
            "/oauth/callback/test",
            false,
            EXCHANGE_TIMEOUT,
            pasted,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "openrouter login failed: HTTP 401");
        assert!(!error.contains("the-code"), "{error}");
        assert!(saved(dir.path(), "openrouter").is_none());
    }

    #[tokio::test]
    async fn login_callback_request_completes_the_login() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (base, _recorded) = keys_server(200, r#"{"key":"sk-or-cb"}"#).await;
        let keys = format!("{base}/api/v1/auth/keys");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let pasted: &'static [u8] = b"";
        let folder = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            openrouter(
                OPENROUTER,
                &folder,
                &keys,
                &listener,
                "/oauth/callback/test",
                false,
                EXCHANGE_TIMEOUT,
                pasted,
            )
            .await
        });
        // A first request with no code is refused, the second one lands.
        for (target, expect) in [
            ("/oauth/callback/test?x=1", "400"),
            ("/oauth/callback/test?code=cb-code", "200"),
        ] {
            let mut socket = connect(port).await;
            socket
                .write_all(format!("GET {target} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes())
                .await
                .expect("writes");
            let mut response = String::new();
            socket.read_to_string(&mut response).await.expect("reads");
            assert!(
                response.starts_with(&format!("HTTP/1.1 {expect}")),
                "{response}"
            );
        }
        task.await
            .expect("the task finished")
            .expect("the login works");
    }

    #[tokio::test]
    async fn login_idle_connection_does_not_block_the_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let pasted: &'static [u8] = b"";
        // The browser opens an idle preconnect first; the callback comes on a
        // second connection, and its code must arrive well before the timeout.
        let flow = async {
            tokio::time::timeout(
                Duration::from_secs(5),
                wait_for_code(Some(&listener), "/oauth/callback/test", pasted),
            )
            .await
            .expect("the code arrives without waiting for the login timeout")
            .expect("the callback works")
        };
        let browser = async {
            let _idle = connect(port).await;
            let mut socket = connect(port).await;
            socket
                .write_all(b"GET /oauth/callback/test?code=idle-code HTTP/1.1\r\n\r\n")
                .await
                .expect("writes");
            let mut response = String::new();
            let _ = socket.read_to_string(&mut response).await;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        };
        let (code, ()) = tokio::join!(flow, browser);
        assert_eq!(code, "idle-code");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[expect(clippy::disallowed_methods, reason = "a test writes its own file")]
    async fn login_a_refused_auth_file_fails_before_the_exchange() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = auth::path(dir.path());
        std::fs::write(
            &path,
            "{\"openrouter\": {\"type\": \"key\", \"key\": \"sk-1\"}}\n",
        )
        .expect("writes");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmods");
        let (base, mut recorded) = keys_server(200, r#"{"key":"sk-or-test"}"#).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let pasted: &'static [u8] = b"http://127.0.0.1:4999/oauth/callback/test?code=the-code\n";
        let error = openrouter(
            OPENROUTER,
            dir.path(),
            &format!("{base}/api/v1/auth/keys"),
            &listener,
            "/oauth/callback/test",
            false,
            EXCHANGE_TIMEOUT,
            pasted,
        )
        .await
        .unwrap_err();
        assert!(error.contains("chmod 600"), "{error}");
        assert!(!error.contains("sk-1"), "{error}");
        assert!(!error.contains("the-code"), "{error}");
        // The code was never traded for a key, so nothing was lost.
        assert!(
            matches!(
                recorded.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "the exchange ran before the auth file was read"
        );
    }

    #[tokio::test]
    async fn login_a_silent_exchange_times_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        // An endpoint that accepts and never answers.
        let keys = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = keys.local_addr().expect("address");
        tokio::spawn(async move {
            let (_socket, _) = keys.accept().await.expect("accept");
            std::future::pending::<()>().await;
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let pasted: &'static [u8] = b"http://127.0.0.1:4999/oauth/callback/test?code=the-code\n";
        let error = openrouter(
            OPENROUTER,
            dir.path(),
            &format!("http://{address}/api/v1/auth/keys"),
            &listener,
            "/oauth/callback/test",
            false,
            Duration::from_millis(100),
            pasted,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "openrouter login timed out");
        assert!(!error.contains("the-code"), "{error}");
        assert!(saved(dir.path(), "openrouter").is_none());
    }

    #[test]
    fn login_logout_drops_a_saved_login() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = auth::Logins::default();
        logins.set(
            "openrouter",
            Login::Key {
                key: Secret::from("sk-or-1"),
            },
        );
        auth::write(dir.path(), &logins).expect("writes");
        assert!(logout_in(dir.path(), "openrouter").expect("logs out"));
        assert!(saved(dir.path(), "openrouter").is_none());
        assert!(auth::read(dir.path()).expect("reads").is_empty());
    }

    #[test]
    fn login_logout_of_an_unsaved_name_says_so_and_keeps_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(!logout_in(dir.path(), "openrouter").expect("logs out"));
        assert!(auth::read(dir.path()).expect("reads").is_empty());
        let mut logins = auth::Logins::default();
        logins.set(
            "openrouter",
            Login::Key {
                key: Secret::from("sk-or-1"),
            },
        );
        auth::write(dir.path(), &logins).expect("writes");
        assert!(!logout_in(dir.path(), "nope").expect("logs out"));
        assert_eq!(saved(dir.path(), "openrouter").as_deref(), Some("sk-or-1"));
    }

    #[test]
    fn login_args_read_the_provider_and_the_browser_flag() {
        let two = |args: &[&str]| -> Result<(String, bool, Option<String>), String> {
            login_args(&args.iter().map(OsString::from).collect::<Vec<_>>())
        };
        assert_eq!(
            two(&["openrouter"]).ok(),
            Some(("openrouter".into(), true, None))
        );
        assert_eq!(two(&["chatgpt"]).ok(), Some(("chatgpt".into(), true, None)));
        assert_eq!(
            two(&["openrouter", "--no-browser"]).ok(),
            Some(("openrouter".into(), false, None))
        );
        let error = two(&[]).unwrap_err();
        assert_eq!(
            error,
            "login needs a provider; login supports: openrouter, chatgpt"
        );
        assert!(two(&["openrouter", "--nope"]).is_err());
        assert!(two(&["a", "b"]).is_err());
    }

    #[test]
    fn login_as_reads_a_name_and_refuses_a_bad_one() {
        let args = |items: &[&str]| -> Vec<OsString> { items.iter().map(OsString::from).collect() };
        let (name, browser, account) =
            login_args(&args(&["chatgpt", "--as", "codex-sub"])).expect("the flags read");
        assert_eq!(name, "chatgpt");
        assert!(browser);
        assert_eq!(account.as_deref(), Some("codex-sub"));
        let error = login_args(&args(&["chatgpt", "--as", "a.b"])).unwrap_err();
        assert!(error.contains("a.b"), "{error}");
        assert!(error.contains("letters, digits, - and _"), "{error}");
        let error = login_args(&args(&["chatgpt", "--as"])).unwrap_err();
        assert!(error.contains("missing value for --as"), "{error}");
    }

    #[tokio::test]
    async fn login_as_saves_the_openrouter_key_under_the_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (base, _recorded) = keys_server(200, r#"{"key":"sk-or-work"}"#).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let pasted: &'static [u8] = b"http://127.0.0.1:4999/oauth/callback/test?code=the-code\n";
        openrouter(
            "or-work",
            dir.path(),
            &format!("{base}/api/v1/auth/keys"),
            &listener,
            "/oauth/callback/test",
            false,
            EXCHANGE_TIMEOUT,
            pasted,
        )
        .await
        .expect("the login works");
        assert_eq!(saved(dir.path(), "or-work").as_deref(), Some("sk-or-work"));
        assert_eq!(saved(dir.path(), OPENROUTER), None);
    }

    /// The end-to-end ChatGPT tests sign an ID token, which needs the test key
    /// of `reagent-ai`; without its `testing` feature they are not compiled.
    #[cfg(feature = "testing")]
    mod chatgpt_tests {
        use reagent_ai::oauth::jwt::testing;

        use super::*;

        /// The query pairs of `url`, percent-decoded.
        fn params(url: &str) -> Vec<(String, String)> {
            let query = url.split_once('?').map(|(_, query)| query).unwrap_or("");
            query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (decoded(name), decoded(value))
                })
                .collect()
        }

        /// `text` with its `%XX` escapes read.
        fn decoded(text: &str) -> String {
            let bytes = text.as_bytes();
            let hex = |at: usize| (bytes[at] as char).to_digit(16);
            let mut out = Vec::with_capacity(bytes.len());
            let mut index = 0;
            while index < bytes.len() {
                if bytes[index] == b'%'
                    && index + 2 < bytes.len()
                    && let (Some(high), Some(low)) = (hex(index + 1), hex(index + 2))
                {
                    out.push((high * 16 + low) as u8);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        }

        fn param(url: &str, name: &str) -> Option<String> {
            params(url)
                .into_iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value)
        }

        /// The pairs of a form body, percent-decoded.
        fn form(body: &str) -> Vec<(String, String)> {
            body.split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (decoded(name), decoded(value))
                })
                .collect()
        }

        /// One ID token payload for the nonce and client id of a login.
        fn id_claims(aud: &str, nonce: &str) -> String {
            format!(
                "{{\"iss\":\"{}\",\"aud\":\"{aud}\",\"nonce\":\"{nonce}\",\"exp\":{},\"sub\":\"user-1\",\"email\":\"a@b.c\"}}",
                oauth::chatgpt::ISSUER,
                now() + 3600
            )
        }

        /// A token reply for `id_token`.
        fn reply(id_token: &str, scope: &str) -> String {
            format!(
                "{{\"access_token\":\"access-1\",\"refresh_token\":\"refresh-1\",\"id_token\":\"{id_token}\",\"token_type\":\"Bearer\",\"expires_in\":3600,\"scope\":\"{scope}\"}}"
            )
        }

        /// Saves a ChatGPT login, as a first login leaves it.
        fn save_oauth(dir: &Path) {
            let mut logins = auth::Logins::default();
            logins.set(
                CHATGPT,
                Login::Oauth {
                    client_id: "oaiapp_test".into(),
                    host_id: "urn:uuid:00000000-0000-4000-8000-000000000000".into(),
                    access: Secret::from("access-1"),
                    refresh: Secret::from("refresh-1"),
                    id_token: Secret::from("id-1"),
                    expires_at: now() + 3600,
                    subject: "user-1".into(),
                    email: None,
                },
            );
            auth::write(dir, &logins).expect("writes the saved login");
        }

        /// The token server: records each request body and answers with
        /// whatever the test sends, so the reply can name the nonce of the URL
        /// the flow printed. It serves the revoke endpoint of a logout too.
        struct Server {
            url: String,
            requests: mpsc::UnboundedReceiver<String>,
            replies: mpsc::UnboundedSender<(u16, String)>,
        }

        impl Server {
            /// The body of the next request the server read.
            async fn next(&mut self) -> String {
                self.requests
                    .recv()
                    .await
                    .expect("the server records the request")
            }

            /// Whether no request arrived.
            fn quiet(&mut self) -> bool {
                matches!(
                    self.requests.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                )
            }

            /// The answer the waiting request gets.
            fn answer(&self, reply: &str) {
                self.answer_with(200, reply);
            }

            /// The answer the waiting request gets, with `status`.
            fn answer_with(&self, status: u16, reply: &str) {
                self.replies
                    .send((status, reply.to_string()))
                    .expect("sends the reply");
            }
        }

        async fn token_server() -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
            let address = listener.local_addr().expect("the bound address");
            let (requested, requests) = mpsc::unbounded_channel();
            let (replies, mut answers) = mpsc::unbounded_channel::<(u16, String)>();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let body = read_body(&mut socket).await;
                    let _ = requested.send(body);
                    let Some((status, reply)) = answers.recv().await else {
                        return;
                    };
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });
            Server {
                url: format!("http://{address}/oauth/token"),
                requests,
                replies,
            }
        }

        /// One login flow, driven by a pasted callback.
        struct Harness {
            input: tokio::io::DuplexStream,
            printed: mpsc::UnboundedReceiver<String>,
            task: Option<tokio::task::JoinHandle<Result<(), String>>>,
        }

        /// Runs one login against `token` and `jwks`, saving under `dir`, with
        /// no callback listener: the port could not be bound.
        async fn start(token: &str, jwks: &str, dir: &Path) -> Harness {
            start_at(token, jwks, dir, None).await
        }

        /// Runs one login against `token` and `jwks`, saving under `dir`, with
        /// `listener` as the callback port when it is given.
        async fn start_at(
            token: &str,
            jwks: &str,
            dir: &Path,
            listener: Option<TcpListener>,
        ) -> Harness {
            let (client, input) = tokio::io::duplex(4096);
            let (sender, printed) = mpsc::unbounded_channel();
            let port = match &listener {
                Some(listener) => listener.local_addr().expect("the bound address").port(),
                None => oauth::chatgpt::PORT,
            };
            let flow = Flow {
                name: CHATGPT.to_string(),
                dir: dir.to_path_buf(),
                endpoints: Endpoints {
                    token: token.to_string(),
                    jwks: jwks.to_string(),
                    revoke: "http://127.0.0.1:1/revoke".to_string(),
                },
                port,
                listener,
                browser: false,
                exchange_timeout: EXCHANGE_TIMEOUT,
            };
            let task = tokio::spawn(async move {
                let announce = move |url: &str| {
                    let _ = sender.send(url.to_string());
                };
                chatgpt_login(&flow, tokio::io::BufReader::new(client), announce).await
            });
            Harness {
                input,
                printed,
                task: Some(task),
            }
        }

        impl Harness {
            /// The URL the flow printed.
            async fn url(&mut self) -> String {
                self.printed.recv().await.expect("the flow prints its URL")
            }

            /// Pastes `callback` as the browser would come back.
            async fn paste(&mut self, callback: &str) {
                self.input
                    .write_all(format!("{callback}\n").as_bytes())
                    .await
                    .expect("writes the paste");
            }

            /// The outcome of the flow.
            async fn finish(&mut self) -> Result<(), String> {
                self.task
                    .take()
                    .expect("one finish")
                    .await
                    .expect("the flow task finished")
            }
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn login_chatgpt_first_login_saves_the_issued_client() {
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let dir = tempfile::tempdir().expect("temp dir");
            let mut run = start(&server.url, &base, dir.path()).await;
            let url = run.url().await;
            // The first login asks for a dynamic client and names this agent.
            assert_eq!(
                param(&url, "client_id").as_deref(),
                Some(oauth::chatgpt::DYNAMIC_CLIENT_ID)
            );
            assert_eq!(param(&url, "agent_name_hint").as_deref(), Some("reagent"));
            assert!(param(&url, "id_token_hint").is_none());
            assert_eq!(param(&url, "response_type").as_deref(), Some("code"));
            assert_eq!(
                param(&url, "code_challenge_method").as_deref(),
                Some("S256")
            );
            let challenge = param(&url, "code_challenge").expect("a challenge");
            assert_eq!(challenge.len(), 43);
            assert_eq!(
                param(&url, "redirect_uri").as_deref(),
                Some("http://127.0.0.1:1455/auth/callback")
            );
            assert_eq!(
                param(&url, "resource").as_deref(),
                Some(oauth::chatgpt::RESOURCE)
            );
            let scope = param(&url, "scope").expect("a scope");
            assert!(scope.contains(oauth::chatgpt::PLAN_SCOPE), "{scope}");
            let nonce = param(&url, "nonce").expect("a nonce");
            let state = param(&url, "state").expect("a state");
            let host = param(&url, "ext_agent_host_id").expect("a host id");
            assert!(host.starts_with("urn:uuid:"), "{host}");
            assert_eq!(host.len(), "urn:uuid:".len() + 36);

            run.paste(&format!(
                "http://127.0.0.1:1455/auth/callback?code=the-code&state={state}&client_id=oaiapp_test"
            ))
            .await;
            let body = server.next().await;
            let form = form(&body);
            let field = |name: &str| {
                form.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.as_str())
            };
            assert_eq!(field("grant_type"), Some("authorization_code"));
            assert_eq!(field("client_id"), Some("oaiapp_test"));
            assert_eq!(field("code"), Some("the-code"));
            let verifier = field("code_verifier").expect("a verifier");
            assert_eq!(verifier.len(), 43);
            assert_eq!(
                field("redirect_uri"),
                Some("http://127.0.0.1:1455/auth/callback")
            );
            assert_eq!(field("resource"), Some(oauth::chatgpt::RESOURCE));
            let id_token = testing::sign(&id_claims("oaiapp_test", &nonce)).expect("signs");
            server.answer(&reply(&id_token, "openid chatgpt.tokens.use.direct"));
            run.finish().await.expect("the login works");
            let logins = auth::read(dir.path()).expect("reads");
            let Some(Login::Oauth {
                client_id,
                host_id,
                access,
                refresh,
                id_token,
                expires_at,
                subject,
                email,
            }) = logins.get(CHATGPT)
            else {
                panic!("an oauth login");
            };
            assert_eq!(client_id, "oaiapp_test");
            assert!(host_id.starts_with("urn:uuid:"), "{host_id}");
            assert_eq!(host_id, &host);
            assert_eq!(access.expose(), "access-1");
            assert_eq!(refresh.expose(), "refresh-1");
            assert!(id_token.expose().starts_with("ey"), "an ID token");
            assert!(*expires_at > now() + 3000);
            assert_eq!(subject, "user-1");
            assert_eq!(email.as_deref(), Some("a@b.c"));
            assert_eq!(mode(&auth::path(dir.path())), 0o600);
        }

        #[tokio::test]
        async fn login_chatgpt_later_login_reuses_client_and_host() {
            let dir = tempfile::tempdir().expect("temp dir");
            save_oauth(dir.path());
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let mut run = start(&server.url, &base, dir.path()).await;
            let url = run.url().await;
            // The later login is the same agent host, and names the saved ID
            // token instead of asking for an agent name.
            assert_eq!(param(&url, "client_id").as_deref(), Some("oaiapp_test"));
            assert_eq!(
                param(&url, "ext_agent_host_id").as_deref(),
                Some("urn:uuid:00000000-0000-4000-8000-000000000000")
            );
            assert_eq!(param(&url, "id_token_hint").as_deref(), Some("id-1"));
            assert!(param(&url, "agent_name_hint").is_none());
            let nonce = param(&url, "nonce").expect("a nonce");
            let state = param(&url, "state").expect("a state");
            run.paste(&format!(
                "http://127.0.0.1:1455/auth/callback?code=two&state={state}&client_id=oaiapp_test"
            ))
            .await;
            let body = server.next().await;
            assert!(body.contains("client_id=oaiapp_test"), "the issued client");
            assert!(body.contains("code=two"), "the new code");
            let id_token = testing::sign(&id_claims("oaiapp_test", &nonce)).expect("signs");
            server.answer(&reply(&id_token, "openid chatgpt.tokens.use.direct"));
            run.finish().await.expect("the later login works");
            let logins = auth::read(dir.path()).expect("reads");
            let Some(Login::Oauth {
                client_id,
                host_id,
                id_token: saved,
                ..
            }) = logins.get(CHATGPT)
            else {
                panic!("an oauth login");
            };
            assert_eq!(client_id, "oaiapp_test");
            assert_eq!(host_id, "urn:uuid:00000000-0000-4000-8000-000000000000");
            assert_eq!(saved.expose(), id_token);
        }

        #[tokio::test]
        async fn login_chatgpt_wrong_state_makes_no_exchange() {
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let dir = tempfile::tempdir().expect("temp dir");
            let mut run = start(&server.url, &base, dir.path()).await;
            let url = run.url().await;
            assert!(param(&url, "state").is_some());
            run.paste(
                "http://127.0.0.1:1455/auth/callback?code=the-code&state=not-the-state&client_id=oaiapp_1",
            )
            .await;
            let error = run.finish().await.unwrap_err();
            assert!(error.contains("state"), "{error}");
            assert!(server.quiet(), "the code was traded for tokens");
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }

        #[tokio::test]
        async fn login_chatgpt_wrong_state_request_keeps_waiting() {
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let dir = tempfile::tempdir().expect("temp dir");
            // A listener of its own, so a page on this machine can reach the
            // callback port.
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let port = listener.local_addr().expect("the bound address").port();
            let mut run = start_at(&server.url, &base, dir.path(), Some(listener)).await;
            let url = run.url().await;
            let state = param(&url, "state").expect("a state");
            let nonce = param(&url, "nonce").expect("a nonce");
            // The first request names a code of its own and the wrong state:
            // it is answered 400 and the login waits on.
            let mut socket = connect(port).await;
            socket
                .write_all(
                    b"GET /auth/callback?code=evil&state=not-the-state HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n",
                )
                .await
                .expect("writes the request");
            let mut response = String::new();
            socket
                .read_to_string(&mut response)
                .await
                .expect("reads the answer");
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
            assert!(server.quiet(), "the wrong state traded a code");
            assert!(auth::read(dir.path()).expect("reads").is_empty());
            // The real callback still lands, and the login succeeds.
            run.paste(&format!(
                "http://127.0.0.1:{port}/auth/callback?code=the-code&state={state}&client_id=oaiapp_1"
            ))
            .await;
            let body = server.next().await;
            assert!(body.contains("code=the-code"), "the pasted code: {body}");
            assert!(!body.contains("evil"), "the refused code: {body}");
            let id_token = testing::sign(&id_claims("oaiapp_1", &nonce)).expect("signs");
            server.answer(&reply(&id_token, "openid chatgpt.tokens.use.direct"));
            run.finish().await.expect("the login still works");
            assert!(
                auth::read(dir.path())
                    .expect("reads")
                    .get(CHATGPT)
                    .is_some()
            );
        }

        #[tokio::test]
        async fn login_chatgpt_denied_callback_stops() {
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let dir = tempfile::tempdir().expect("temp dir");
            let mut run = start(&server.url, &base, dir.path()).await;
            let url = run.url().await;
            let state = param(&url, "state").expect("a state");
            run.paste(&format!(
                "http://127.0.0.1:1455/auth/callback?error=access_denied&state={state}"
            ))
            .await;
            let error = run.finish().await.unwrap_err();
            assert_eq!(error, "login was denied");
            assert!(server.quiet(), "the denied login traded a code");
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }

        #[tokio::test]
        async fn login_chatgpt_missing_plan_scope_is_refused() {
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let dir = tempfile::tempdir().expect("temp dir");
            let mut run = start(&server.url, &base, dir.path()).await;
            let url = run.url().await;
            let nonce = param(&url, "nonce").expect("a nonce");
            let state = param(&url, "state").expect("a state");
            run.paste(&format!(
                "http://127.0.0.1:1455/auth/callback?code=the-code&state={state}&client_id=oaiapp_1"
            ))
            .await;
            let _body = server.next().await;
            let id_token = testing::sign(&id_claims("oaiapp_1", &nonce)).expect("signs");
            server.answer(&reply(&id_token, "openid profile email"));
            let error = run.finish().await.unwrap_err();
            assert_eq!(error, "this ChatGPT account did not grant plan use");
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }

        #[tokio::test]
        async fn login_chatgpt_bad_id_token_is_refused() {
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let dir = tempfile::tempdir().expect("temp dir");
            let mut run = start(&server.url, &base, dir.path()).await;
            let url = run.url().await;
            let state = param(&url, "state").expect("a state");
            run.paste(&format!(
                "http://127.0.0.1:1455/auth/callback?code=the-code&state={state}&client_id=oaiapp_test"
            ))
            .await;
            let _body = server.next().await;
            // A signed token for another nonce: the signature passes and the
            // claim check refuses it.
            let id_token = testing::sign(&id_claims("oaiapp_test", "other-nonce")).expect("signs");
            server.answer(&reply(&id_token, "chatgpt.tokens.use.direct"));
            let error = run.finish().await.unwrap_err();
            assert!(error.contains("nonce"), "the token was refused");
            assert!(!error.contains(&id_token), "the error names the ID token");
            assert!(!error.contains("access-1"), "the error names a token");
            assert!(!error.contains("refresh-1"), "the error names a token");
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }

        #[tokio::test]
        async fn login_chatgpt_a_token_for_another_audience_is_refused() {
            let mut server = token_server().await;
            let (base, _keys) = keys_server(200, testing::JWKS).await;
            let dir = tempfile::tempdir().expect("temp dir");
            let mut run = start(&server.url, &base, dir.path()).await;
            let url = run.url().await;
            let nonce = param(&url, "nonce").expect("a nonce");
            let state = param(&url, "state").expect("a state");
            run.paste(&format!(
                "http://127.0.0.1:1455/auth/callback?code=the-code&state={state}&client_id=oaiapp_1"
            ))
            .await;
            let _body = server.next().await;
            let id_token = testing::sign(&id_claims("oaiapp_other", &nonce)).expect("signs");
            server.answer(&reply(&id_token, "chatgpt.tokens.use.direct"));
            let error = run.finish().await.unwrap_err();
            assert!(error.contains("aud"), "{error}");
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }

        #[tokio::test]
        async fn login_chatgpt_logout_revokes_and_removes() {
            let dir = tempfile::tempdir().expect("temp dir");
            save_oauth(dir.path());
            let mut server = token_server().await;
            let endpoints = Endpoints {
                token: server.url.clone(),
                jwks: server.url.clone(),
                revoke: server.url.clone(),
            };
            // The revoke answers before the logout can return, so the request
            // is read and answered from this task.
            let path = dir.path().to_path_buf();
            let logout = tokio::spawn(async move { logout_with(&path, CHATGPT, &endpoints).await });
            let body = server.next().await;
            assert!(body.contains("token=refresh-1"), "the refresh token");
            assert!(body.contains("token_type_hint=refresh_token"), "the hint");
            assert!(body.contains("client_id=oaiapp_test"), "the client");
            server.answer("");
            let removed = logout.await.expect("the task finished").expect("logs out");
            assert!(removed);
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }

        #[tokio::test]
        async fn login_chatgpt_logout_removes_even_when_revoke_fails() {
            let dir = tempfile::tempdir().expect("temp dir");
            save_oauth(dir.path());
            let mut server = token_server().await;
            let endpoints = Endpoints {
                token: server.url.clone(),
                jwks: server.url.clone(),
                revoke: server.url.clone(),
            };
            let path = dir.path().to_path_buf();
            let logout = tokio::spawn(async move { logout_with(&path, CHATGPT, &endpoints).await });
            let _body = server.next().await;
            server.answer_with(500, r#"{"error":"server_error"}"#);
            let removed = logout.await.expect("the task finished").expect("logs out");
            assert!(removed);
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }

        #[tokio::test]
        async fn login_chatgpt_logout_gives_up_on_a_silent_revoke() {
            let dir = tempfile::tempdir().expect("temp dir");
            save_oauth(dir.path());
            // A revoke endpoint that takes the request and never answers.
            let silent = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = silent.local_addr().expect("the bound address");
            tokio::spawn(async move {
                let (_socket, _) = silent.accept().await.expect("accept");
                std::future::pending::<()>().await;
            });
            let endpoints = Endpoints {
                token: format!("http://{address}/token"),
                jwks: format!("http://{address}/jwks"),
                revoke: format!("http://{address}/revoke"),
            };
            let error = revoke(
                &endpoints,
                "oaiapp_test",
                "refresh-1",
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
            assert_eq!(error, "chatgpt logout failed: the revoke timed out");
            assert!(!error.contains("refresh-1"), "the token is shown");
            // The saved login goes even when the endpoint refuses the call.
            let refused = Endpoints {
                token: endpoints.token.clone(),
                jwks: endpoints.jwks.clone(),
                revoke: "http://127.0.0.1:1/revoke".to_string(),
            };
            let removed = logout_with(dir.path(), CHATGPT, &refused)
                .await
                .expect("logs out");
            assert!(removed);
            assert!(auth::read(dir.path()).expect("reads").is_empty());
        }
    }
}
