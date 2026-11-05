//! A fake MCP server over streamable HTTP, hand-written on a `TcpListener`.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reagent_oauth::callback_query;
use reagent_oauth::loopback::field;
use reagent_oauth::percent_encode;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::sleep;

/// The modern revision the server reports.
const MODERN: &str = "2026-07-28";

/// The legacy revision the server reports.
const LEGACY: &str = "2025-11-25";

/// How long the server says its tool list stays fresh.
const TTL_MS: u64 = 600_000;

/// The id of the `ping` request the server sends on a stream.
const PING_ID: u64 = 9001;

/// The path of the MCP endpoint, which is also the resource path.
pub const MCP_PATH: &str = "/mcp";

/// The path of the protected resource metadata, before the resource path.
const RESOURCE_PATH: &str = "/.well-known/oauth-protected-resource";

/// The paths the authorization server metadata may be served at.
const SERVER_PATHS: [&str; 2] = [
    "/.well-known/oauth-authorization-server",
    "/.well-known/openid-configuration",
];

/// The place a document writes this server's origin in.
const ORIGIN: &str = "{origin}";

/// The place a document writes this server's port in.
pub const PORT: &str = "{port}";

/// The path of the dynamic registration endpoint.
const REGISTER_PATH: &str = "/register";

/// The code the authorize endpoint sends to the callback.
const CODE: &str = "the-code";

/// The client id the registration endpoint issues.
const CLIENT_ID: &str = "dcr-client";

/// The token a revocation leaves in place, which no request carries.
const REVOKED: &str = "revoked";

/// Which sign-in the `invalid_client` switch refuses.
const REFUSED_SIGN_IN: usize = 2;

/// The bytes of one MiB.
const MIB: usize = 1024 * 1024;

/// How long a `DELETE` the server never answers holds its connection.
const SILENT_SECS: u64 = 30;

/// The lifetime a token reply names when the test says nothing.
const LIFETIME: u64 = 3600;

/// A tool of the fake list: its name and its input schema.
pub fn tool(name: &str, input_schema: Value) -> (String, Value) {
    (name.to_owned(), input_schema)
}

/// One request the server read.
#[derive(Clone, Debug)]
pub struct Request {
    /// The HTTP method of the request line, such as `POST`.
    pub method: String,
    /// The path of the request line.
    pub path: String,
    /// The header names, lowercased, with their values.
    pub headers: Vec<(String, String)>,
    /// The body, as text.
    pub body: String,
}

impl Request {
    /// The value of the header `name`, which is lowercase.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(seen, _)| seen == name)
            .map(|(_, value)| value.as_str())
    }

    /// The JSON-RPC message of the body, when the body holds one.
    pub fn message(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    /// The JSON-RPC method of the body, when it has one.
    pub fn rpc(&self) -> Option<String> {
        let message = self.message()?;
        message.get("method")?.as_str().map(str::to_owned)
    }
}

/// How the fake answers.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Speak the legacy era: a `400` to the probe, then `initialize`.
    pub legacy: bool,
    /// Answer `tools/call` as an event stream.
    pub sse: bool,
    /// Send a `ping` request on the event stream before the answer.
    pub ping_before_call: bool,
    /// Answer the next request of this JSON-RPC method with this status.
    pub fail: Option<(String, u16)>,
    /// Answer the first `tools/call` with a `-32020` header mismatch.
    pub mismatch_first: bool,
    /// Answer the first request of its own (not a notification) that carries
    /// a session with a `404`, and give each `initialize` a new session id made
    /// of `session` and a count.
    pub expire_session: bool,
    /// Answer every request with a `401` and this `WWW-Authenticate`.
    pub unauthorized: Option<String>,
    /// Answer every request like the `2024-11-05` HTTP+SSE transport: a stream
    /// that first names an endpoint.
    pub old_sse: bool,
    /// Wait this long in the middle of an event stream, in milliseconds.
    pub slow_ms: Option<u64>,
    /// The session id a legacy `initialize` answers with.
    pub session: Option<String>,
    /// The tools of the list, name and input schema.
    pub tools: Vec<(String, Value)>,
    /// Never answer a `DELETE`, so the client's stop waits for its timeout.
    pub silent_delete: bool,
    /// Answer `tools/list` with a body of this many MiB.
    pub huge_body_mib: Option<usize>,
    /// Serve the OAuth endpoints and demand a bearer token.
    pub oauth: Option<Oauth>,
    /// Answer a `POST` at the MCP endpoint with this status and this
    /// `Location`; `{origin}` and `{port}` name this server's own.
    pub redirect: Option<(u16, String)>,
    /// Wait this long before answering a `tools/list` past the first one, in
    /// milliseconds.
    pub list_delay_ms: Option<u64>,
    /// The `ttlMs` of a `tools/list` reply; the default when unset.
    pub list_ttl_ms: Option<u64>,
}

/// The OAuth side of a fake server: a protected resource, an authorization
/// server, its registration and token endpoints, on the MCP origin.
///
/// Every text may write `{origin}` for the origin of this server.
#[derive(Clone, Debug)]
pub struct Oauth {
    /// Whether the `401` names the metadata URL in `WWW-Authenticate`.
    pub announce: bool,
    /// The `scope` the `401` asks for.
    pub challenge_scope: Option<String>,
    /// The issuer the resource metadata names.
    pub issuer: String,
    /// The `iss` of the callback redirect; the issuer when unset.
    pub iss: Option<String>,
    /// The `code_challenge_methods_supported` of the metadata document.
    pub methods: Vec<String>,
    /// Whether the metadata says the callback carries `iss`.
    pub iss_parameter: bool,
    /// Whether the metadata says a client id metadata document is accepted.
    pub client_metadata: bool,
    /// Whether the metadata names a registration endpoint.
    pub registration: bool,
    /// The `expires_in` of a token reply.
    pub expires_in: u64,
    /// The `refresh_token` an authorization code reply carries.
    pub refresh: Option<String>,
    /// The `scope` of a token reply.
    pub scope: String,
    /// The `scopes_supported` of the resource metadata.
    pub scopes: Vec<String>,
    /// The `scopes_supported` of the authorization server metadata.
    pub server_scopes: Vec<String>,
    /// The `scope` a tool call is refused for with `insufficient_scope`.
    pub step_up: Option<String>,
    /// The access token a token reply is built on.
    pub access: String,
    /// Documents served at other paths, verbatim but for `{origin}`.
    pub extra: Vec<(String, String)>,
    /// Forget the token once this many requests reached the endpoint, so the
    /// requests that carry it are refused until a refresh buys a new one.
    pub revoke_after: Option<usize>,
    /// Answer the second `authorization_code` with `invalid_client`, the way a
    /// server that forgot a registered client does.
    pub invalid_client: bool,
}

impl Default for Oauth {
    fn default() -> Self {
        Oauth {
            announce: true,
            challenge_scope: None,
            issuer: ORIGIN.to_string(),
            iss: None,
            methods: vec!["S256".to_string()],
            iss_parameter: false,
            client_metadata: false,
            registration: true,
            expires_in: LIFETIME,
            refresh: Some("refresh-1".to_string()),
            scope: "files.read".to_string(),
            scopes: vec!["files.read".to_string()],
            server_scopes: vec!["files.read".to_string(), "offline_access".to_string()],
            step_up: None,
            access: "access".to_string(),
            extra: Vec::new(),
            revoke_after: None,
            invalid_client: false,
        }
    }
}

/// A running fake server on `127.0.0.1`.
pub struct Server {
    /// The URL every message is posted to.
    pub url: String,
    /// The `scheme://host:port` of the server, which is also an issuer.
    pub origin: String,
    state: Arc<State>,
}

impl Server {
    /// Binds a free port and starts answering.
    pub async fn start(settings: Settings) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let address = listener.local_addr().expect("the bound address");
        let origin = format!("http://{address}");
        let state = Arc::new(State {
            settings,
            requests: Mutex::new(Vec::new()),
            failed: AtomicUsize::new(0),
            sessions: AtomicUsize::new(0),
            origin,
            issued: Mutex::new(Vec::new()),
            tokens: AtomicUsize::new(0),
            reached: AtomicUsize::new(0),
            revoked: AtomicBool::new(false),
            sign_ins: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
        });
        let serving = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let state = Arc::clone(&serving);
                tokio::spawn(async move { handle(socket, state).await });
            }
        });
        Server {
            url: format!("http://{address}{MCP_PATH}"),
            origin: format!("http://{address}"),
            state,
        }
    }

    /// Every request so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.state
            .requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }

    /// The requests whose JSON-RPC method is `method`.
    pub fn calls(&self, method: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.rpc().as_deref() == Some(method))
            .collect()
    }

    /// The last request whose JSON-RPC method is `method`.
    pub fn call(&self, method: &str) -> Option<Request> {
        self.calls(method).pop()
    }

    /// The requests with the HTTP method `method`, such as `DELETE`.
    pub fn http(&self, method: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.method == method)
            .collect()
    }

    /// The path, without its query, of each request so far.
    pub fn paths(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| {
                request
                    .path
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    /// The decoded query or form pairs of each request whose path is `path`.
    pub fn pairs(&self, path: &str, name: &str) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|request| request.path.split('?').next() == Some(path))
            .filter_map(|request| pair_of(&request, name))
            .collect()
    }

    /// How many dynamic registrations reached the registration endpoint.
    pub fn registrations(&self) -> usize {
        self.requests()
            .into_iter()
            .filter(|request| request.path.split('?').next() == Some(REGISTER_PATH))
            .count()
    }
}

/// The value of `name` in the query of `request`, or in its form body.
fn pair_of(request: &Request, name: &str) -> Option<String> {
    let path = request.path.split('?').next().unwrap_or_default();
    if let Some(pairs) = callback_query(&request.path, path)
        && let Some(value) = field(&pairs, name)
    {
        return Some(value.to_string());
    }
    let form = callback_query(&format!("/?{}", request.body), "/")?;
    field(&form, name).map(str::to_owned)
}

/// The state the connection tasks share.
struct State {
    settings: Settings,
    requests: Mutex<Vec<Request>>,
    failed: AtomicUsize,
    sessions: AtomicUsize,
    origin: String,
    issued: Mutex<Vec<String>>,
    tokens: AtomicUsize,
    reached: AtomicUsize,
    revoked: AtomicBool,
    sign_ins: AtomicUsize,
    lists: AtomicUsize,
}

/// Whether the message waits for an answer, so it is a request of its own.
fn answers(message: &Option<Value>) -> bool {
    message
        .as_ref()
        .is_some_and(|message| message.get("id").is_some_and(|id| !id.is_null()))
}

/// Reads one request and answers it.
async fn handle(mut socket: TcpStream, state: Arc<State>) {
    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    let http_method = request.method.clone();
    let carried_session = request.header("mcp-session-id").is_some();
    let message = request.message();
    if let Ok(mut requests) = state.requests.lock() {
        requests.push(request.clone());
    }
    if http_method == "DELETE" {
        if state.settings.silent_delete {
            // Left open and unanswered, the way a stuck server behaves.
            sleep(Duration::from_secs(SILENT_SECS)).await;
            return;
        }
        respond(&mut socket, 204, &[], "").await;
        return;
    }
    if let Some((status, location)) = state.settings.redirect.clone()
        && http_method == "POST"
        && request.path.split('?').next() == Some(MCP_PATH)
    {
        let location = fill_port(&fill(&location, &state.origin), &state.origin);
        respond(&mut socket, status, &[("location", &location)], "").await;
        return;
    }
    if let Some(oauth) = state.settings.oauth.clone() {
        let path = request.path.split('?').next().unwrap_or_default();
        if path != MCP_PATH {
            oauth_route(&mut socket, &state, &oauth, &request).await;
            return;
        }
        revoke(&state, &oauth);
        if !authorized(&state, &oauth, &request) {
            let challenge = challenge(&state, &oauth);
            respond(&mut socket, 401, &[("www-authenticate", &challenge)], "").await;
            return;
        }
    }
    if let Some(authenticate) = state.settings.unauthorized.clone() {
        respond(&mut socket, 401, &[("www-authenticate", &authenticate)], "").await;
        return;
    }
    if state.settings.expire_session
        && carried_session
        && answers(&message)
        && state.failed.swap(1, Ordering::SeqCst) == 0
    {
        respond(&mut socket, 404, &[], "").await;
        return;
    }
    if state.settings.old_sse {
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let event = "event: endpoint\ndata: /messages\n\n";
        let _ = socket.write_all(event.as_bytes()).await;
        let _ = socket.shutdown().await;
        return;
    }
    let Some(message) = message else {
        respond(&mut socket, 400, &[], "").await;
        return;
    };
    let method = message["method"].as_str().unwrap_or_default().to_owned();
    if let Some((fail_method, status)) = state.settings.fail.clone()
        && fail_method == method
        && state.failed.swap(1, Ordering::SeqCst) == 0
    {
        respond(&mut socket, status, &[], "").await;
        return;
    }
    let Some(id) = message.get("id").cloned().filter(|id| !id.is_null()) else {
        respond(&mut socket, 202, &[], "").await;
        return;
    };
    match method.as_str() {
        "server/discover" => {
            if state.settings.legacy {
                respond(&mut socket, 400, &[], "").await;
            } else {
                json_reply(&mut socket, &discover(&id)).await;
            }
        }
        "initialize" => {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": LEGACY,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "0"},
                },
            });
            let session = state.settings.session.as_deref().map(|session| {
                let count = state.sessions.fetch_add(1, Ordering::SeqCst) + 1;
                if state.settings.expire_session {
                    format!("{session}{count}")
                } else {
                    session.to_owned()
                }
            });
            let mut owned = vec![("content-type", "application/json")];
            if let Some(session) = session.as_deref() {
                owned.push(("mcp-session-id", session));
            }
            respond(&mut socket, 200, &owned, &reply.to_string()).await;
        }
        "tools/list" => {
            let listed = state.lists.fetch_add(1, Ordering::SeqCst);
            if listed > 0
                && let Some(ms) = state.settings.list_delay_ms
            {
                sleep(Duration::from_millis(ms)).await;
            }
            if let Some(mib) = state.settings.huge_body_mib {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"tools": [], "pad": padding(mib)},
                });
                json_reply(&mut socket, &reply).await;
            } else {
                json_reply(&mut socket, &list(&state.settings, &id)).await;
            }
        }
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or_default();
            if let Some(scope) = state
                .settings
                .oauth
                .as_ref()
                .and_then(|oauth| oauth.step_up.clone())
            {
                let challenge = format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\"");
                respond(&mut socket, 403, &[("www-authenticate", &challenge)], "").await;
            } else if state.settings.mismatch_first && state.failed.swap(1, Ordering::SeqCst) == 0 {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32020, "message": "header mismatch"},
                });
                json_reply(&mut socket, &reply).await;
            } else if state.settings.sse {
                stream(&mut socket, &state, &id, name).await;
            } else {
                json_reply(&mut socket, &called(&id, name)).await;
            }
        }
        other => {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {other}")},
            });
            json_reply(&mut socket, &reply).await;
        }
    }
}

/// The result of the modern probe.
fn discover(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "resultType": "complete",
            "supportedVersions": [MODERN],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "0"},
        },
    })
}

/// One page of the tool list.
fn list(settings: &Settings, id: &Value) -> Value {
    let tools: Vec<Value> = settings
        .tools
        .iter()
        .map(|(name, schema)| {
            json!({
                "name": name,
                "description": format!("the {name} tool"),
                "inputSchema": schema,
            })
        })
        .collect();
    let mut result = json!({"tools": tools});
    if !settings.legacy {
        result["resultType"] = json!("complete");
        result["ttlMs"] = json!(settings.list_ttl_ms.unwrap_or(TTL_MS));
    }
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// The result of one tool call.
fn called(id: &Value, name: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"content": [{"type": "text", "text": format!("the {name} tool ran")}]},
    })
}

/// Answers a call with a progress event, a comment and the result.
async fn stream(socket: &mut TcpStream, state: &State, id: &Value, name: &str) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let _ = socket.write_all(head.as_bytes()).await;
    let progress = json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {"progressToken": "p", "progress": 1},
    });
    let _ = socket
        .write_all(format!("event: message\ndata: {progress}\n\n").as_bytes())
        .await;
    let _ = socket.write_all(b": a comment\n\n").await;
    if state.settings.ping_before_call {
        let ping = json!({"jsonrpc": "2.0", "id": PING_ID, "method": "ping"});
        let _ = socket
            .write_all(format!("data: {ping}\n\n").as_bytes())
            .await;
    }
    let _ = socket.flush().await;
    if let Some(ms) = state.settings.slow_ms {
        sleep(Duration::from_millis(ms)).await;
    }
    let result = called(id, name);
    let _ = socket
        .write_all(format!("data: {result}\n\n").as_bytes())
        .await;
    let _ = socket.shutdown().await;
}

/// Answers one message with a `200` and its JSON.
async fn json_reply(socket: &mut TcpStream, message: &Value) {
    respond(
        socket,
        200,
        &[("content-type", "application/json")],
        &message.to_string(),
    )
    .await;
}

/// Writes one response and closes the connection.
async fn respond(socket: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &str) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    head.push_str(body);
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// The end of the request head, past the blank line.
fn head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// The `content-length` of a request head, or 0.
fn content_length(head: &str) -> usize {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Reads one request, head and body.
async fn read_request(socket: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let split = loop {
        if let Some(at) = head_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..at]).into_owned();
            if buffer.len() >= at + 4 + content_length(&head) {
                break at;
            }
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
    let body = String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?.to_owned();
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

/// The `WWW-Authenticate` a request without the token is refused with.
fn challenge(state: &State, oauth: &Oauth) -> String {
    let mut text = String::from("Bearer");
    if oauth.announce {
        text.push_str(&format!(
            " resource_metadata=\"{}{RESOURCE_PATH}{MCP_PATH}\"",
            state.origin
        ));
    }
    if let Some(scope) = &oauth.challenge_scope {
        text.push_str(&format!(", scope=\"{scope}\""));
    }
    text
}

/// Forgets the tokens once the endpoint has answered `revoke_after` requests.
fn revoke(state: &State, oauth: &Oauth) {
    let Some(after) = oauth.revoke_after else {
        return;
    };
    let reached = state.reached.fetch_add(1, Ordering::SeqCst) + 1;
    if reached > after
        && !state.revoked.swap(true, Ordering::SeqCst)
        && let Ok(mut issued) = state.issued.lock()
    {
        issued.clear();
        issued.push(REVOKED.to_owned());
    }
}

/// `mib` MiB of filler, so a reply body goes past the client's read cap.
fn padding(mib: usize) -> String {
    "x".repeat(mib * MIB)
}

/// Whether the request carries a token this server handed out.
fn authorized(state: &State, oauth: &Oauth, request: &Request) -> bool {
    let Some(header) = request.header("authorization") else {
        return false;
    };
    let Ok(issued) = state.issued.lock() else {
        return false;
    };
    if issued.is_empty() {
        return header == format!("Bearer {}", oauth.access);
    }
    issued
        .iter()
        .any(|token| header == format!("Bearer {token}"))
}

/// `text` with the origin of this server written in place of `{origin}`.
fn fill(text: &str, origin: &str) -> String {
    text.replace(ORIGIN, origin)
}

/// `text` with the port of this server written in place of `{port}`.
fn fill_port(text: &str, origin: &str) -> String {
    let port = origin.rsplit(':').next().unwrap_or_default();
    text.replace(PORT, port)
}

/// The document served at `path`, when the OAuth side has one.
fn document(oauth: &Oauth, path: &str, origin: &str) -> Option<String> {
    if let Some((_, body)) = oauth.extra.iter().find(|(at, _)| at == path) {
        return Some(fill(body, origin));
    }
    if path == format!("{RESOURCE_PATH}{MCP_PATH}") || path == RESOURCE_PATH {
        let resource = json!({
            "resource": format!("{origin}{MCP_PATH}"),
            "authorization_servers": [oauth.issuer.clone()],
            "scopes_supported": oauth.scopes,
        });
        return Some(fill(&resource.to_string(), origin));
    }
    if SERVER_PATHS.iter().any(|at| path.starts_with(at)) || path.ends_with(SERVER_PATHS[1]) {
        return Some(fill(&server_document(oauth, origin), origin));
    }
    None
}

/// The authorization server metadata of this fake.
fn server_document(oauth: &Oauth, origin: &str) -> String {
    let mut document = json!({
        "issuer": oauth.issuer.clone(),
        "authorization_endpoint": format!("{origin}/authorize"),
        "token_endpoint": format!("{origin}/token"),
        "code_challenge_methods_supported": oauth.methods,
        "scopes_supported": oauth.server_scopes,
    });
    if oauth.methods.is_empty() {
        document
            .as_object_mut()
            .expect("a document built from an object")
            .remove("code_challenge_methods_supported");
    }
    if oauth.registration {
        document["registration_endpoint"] = json!(format!("{origin}/register"));
    }
    if oauth.iss_parameter {
        document["authorization_response_iss_parameter_supported"] = json!(true);
    }
    if oauth.client_metadata {
        document["client_id_metadata_document_supported"] = json!(true);
    }
    document.to_string()
}

/// Answers one request to a path that is not the MCP endpoint.
async fn oauth_route(socket: &mut TcpStream, state: &State, oauth: &Oauth, request: &Request) {
    let path = request.path.split('?').next().unwrap_or_default();
    match (request.method.as_str(), path) {
        ("GET", "/authorize") => authorize(socket, state, oauth, request).await,
        ("POST", "/token") => token(socket, state, oauth, request).await,
        ("POST", REGISTER_PATH) => registration(socket).await,
        ("GET", _) => match document(oauth, path, &state.origin) {
            Some(text) => {
                respond(socket, 200, &[("content-type", "application/json")], &text).await;
            }
            None => respond(socket, 404, &[], "").await,
        },
        _ => respond(socket, 404, &[], "").await,
    }
}

/// Redirects to the callback the authorize URL named.
async fn authorize(socket: &mut TcpStream, state: &State, oauth: &Oauth, request: &Request) {
    let query = callback_query(&request.path, "/authorize").unwrap_or_default();
    let state_value = field(&query, "state").unwrap_or_default();
    let redirect = field(&query, "redirect_uri").unwrap_or_default();
    let mut location = format!("{redirect}?code={CODE}&state={state_value}");
    let iss = fill(oauth.iss.as_deref().unwrap_or(&oauth.issuer), &state.origin);
    location.push_str(&format!("&iss={}", percent_encode(&iss)));
    respond(socket, 302, &[("location", &location)], "").await;
}

/// Hands out an access token, and a refresh token for a first login.
async fn token(socket: &mut TcpStream, state: &State, oauth: &Oauth, request: &Request) {
    let form = callback_query(&format!("/?{}", request.body), "/").unwrap_or_default();
    if field(&form, "grant_type") == Some("authorization_code")
        && oauth.invalid_client
        && state.sign_ins.fetch_add(1, Ordering::SeqCst) + 1 == REFUSED_SIGN_IN
    {
        let reply = json!({"error": "invalid_client"});
        respond(
            socket,
            400,
            &[("content-type", "application/json")],
            &reply.to_string(),
        )
        .await;
        return;
    }
    let count = state.tokens.fetch_add(1, Ordering::SeqCst) + 1;
    let access = format!("{}-{count}", oauth.access);
    if let Ok(mut issued) = state.issued.lock() {
        issued.push(access.clone());
    }
    let mut reply = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": oauth.expires_in,
        "scope": oauth.scope,
    });
    if field(&form, "grant_type") == Some("authorization_code")
        && let Some(refresh) = &oauth.refresh
    {
        reply["refresh_token"] = json!(refresh);
    }
    respond(
        socket,
        200,
        &[("content-type", "application/json")],
        &reply.to_string(),
    )
    .await;
}

/// Issues a client id for a dynamic registration.
async fn registration(socket: &mut TcpStream) {
    let reply = json!({"client_id": CLIENT_ID});
    respond(
        socket,
        200,
        &[("content-type", "application/json")],
        &reply.to_string(),
    )
    .await;
}
