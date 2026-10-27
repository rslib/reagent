//! One server over stdio or HTTP: its era, its tools and its calls.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::http::{self, HeaderParam, HttpSpec};
use crate::process_group::{self, GroupGuard};
use crate::protocol::{self, Era, Params};

/// How long the modern probe may take before the server counts as legacy.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `stop` waits for the server's reader to end.
const STOP_LIMIT: Duration = Duration::from_secs(2);

/// The largest body, event or line this client reads from one server, in MiB.
pub(crate) const READ_CAP_MIB: usize = 16;

/// The largest body, event or line this client reads from one server.
pub(crate) const READ_CAP: usize = READ_CAP_MIB * 1024 * 1024;

/// How much of the server's stderr is kept for error text.
const STDERR_TAIL: usize = 4 * 1024;

/// How many frames the writer joins into one write.
const WRITE_BATCH: usize = 16;

/// How to start one server process.
#[derive(Clone, Debug, Default)]
pub struct StdioSpec {
    /// The program to start.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Variables added to its environment.
    pub env: Vec<(String, String)>,
    /// The folder it starts in.
    pub cwd: Option<PathBuf>,
}

/// One tool a server offers.
#[derive(Clone, Debug)]
pub struct Tool {
    /// The name a call uses.
    pub name: String,
    /// What the tool does, for the model.
    pub description: Option<String>,
    /// The JSON Schema of its arguments.
    pub input_schema: Value,
    /// The JSON Schema of its structured result, when it has one.
    pub output_schema: Option<Value>,
    /// Hints such as `readOnlyHint`.
    pub annotations: Option<Value>,
    /// The arguments a call sends as `Mcp-Param-*` headers.
    pub(crate) headers: Arc<[HeaderParam]>,
}

/// The result of one tool call.
#[derive(Debug)]
pub struct CallOutput {
    /// The text items joined by newlines, other items named on their own line.
    pub text: String,
    /// The structured result, when the tool sends one.
    pub structured: Option<Value>,
    /// Whether the server marked the result as an error.
    pub is_error: bool,
}

/// One running server, spoken to over stdio or streamable HTTP.
pub struct Client {
    transport: Transport,
    era: Era,
    /// The tool list, read and replaced without holding it over the network.
    tools: RwLock<Option<Arc<Listed>>>,
    /// Tells the relists apart, so two of them never run at once.
    relisting: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("era", &self.era)
            .finish_non_exhaustive()
    }
}

/// Where the messages of one server go.
enum Transport {
    Stdio(Process),
    Http(http::Http),
}

/// One running server process and its pipes.
struct Process {
    wire: Arc<Wire>,
    reader: JoinHandle<()>,
    group: GroupGuard,
}

impl Process {
    /// Whether the server already ended and was reaped, so its group id may
    /// belong to another group by now.
    fn reaped(&self) -> bool {
        self.wire.served().gone.is_some()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.reaped() {
            self.group.disarm();
        }
    }
}

impl Client {
    /// Starts the server, finds its era, and leaves the tool list for later.
    pub async fn stdio(spec: &StdioSpec) -> Result<Client, Error> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .envs(spec.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        process_group::new_session(&mut command);
        let mut child = command.spawn().map_err(|error| Error::Spawn {
            command: spec.command.clone(),
            reason: error.to_string(),
        })?;
        let (Some(stdin), Some(stdout), Some(stderr), Some(pid)) = (
            child.stdin.take(),
            child.stdout.take(),
            child.stderr.take(),
            child.id(),
        ) else {
            return Err(Error::Spawn {
                command: spec.command.clone(),
                reason: "the process has no pipes".to_owned(),
            });
        };

        let group = GroupGuard::new(pid as i32);
        let (out, frames) = mpsc::unbounded_channel();
        let wire = Arc::new(Wire {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            stale: AtomicBool::new(false),
            state: Mutex::new(Served::default()),
        });
        tokio::spawn(write_frames(stdin, frames, Arc::clone(&wire)));
        tokio::spawn(drain_stderr(stderr, Arc::clone(&wire)));
        let reader = tokio::spawn(read_lines(child, stdout, Arc::clone(&wire)));

        let era = detect(&wire).await?;
        Ok(Client {
            transport: Transport::Stdio(Process {
                wire,
                reader,
                group,
            }),
            era,
            tools: RwLock::new(None),
            relisting: tokio::sync::Mutex::new(()),
        })
    }

    /// Connects over streamable HTTP, finds its era, and leaves the tool list
    /// for later.
    pub async fn http(spec: &HttpSpec) -> Result<Client, Error> {
        let (server, era) = http::Http::connect(spec).await?;
        Ok(Client {
            transport: Transport::Http(server),
            era,
            tools: RwLock::new(None),
            relisting: tokio::sync::Mutex::new(()),
        })
    }

    /// The server's tools, cached until a list change or the ttl lapses.
    ///
    /// A call reads the list under a lock held for one pointer copy, so it
    /// never waits for a relist; the relists wait for each other instead.
    pub async fn tools(&self) -> Result<Arc<[Tool]>, Error> {
        if let Some(list) = self.current() {
            return Ok(list);
        }
        let _relisting = self.relisting.lock().await;
        if let Some(list) = self.current() {
            return Ok(list);
        }
        // Cleared before the request, so a change during it marks the list
        // stale again instead of being lost; a failed or dropped relist marks
        // it stale again too, so the change is taken by the next one.
        self.set_stale(false);
        let mut restore = Restale(Some(self));
        let (tools, ttl) = self.list().await?;
        let listed = Arc::new(Listed::new(tools, ttl));
        let list = Arc::clone(&listed.tools);
        *self.listed_write() = Some(listed);
        restore.0 = None;
        Ok(list)
    }

    /// The tool list, when the server's list did not change and its ttl holds.
    fn current(&self) -> Option<Arc<[Tool]>> {
        if self.stale() {
            return None;
        }
        let listed = self.listed_read();
        listed.as_ref().and_then(|listed| listed.fresh())
    }

    fn listed_read(&self) -> RwLockReadGuard<'_, Option<Arc<Listed>>> {
        self.tools.read().unwrap_or_else(|error| error.into_inner())
    }

    fn listed_write(&self) -> RwLockWriteGuard<'_, Option<Arc<Listed>>> {
        self.tools
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Calls one tool with the arguments text the model wrote.
    pub async fn call(
        &self,
        name: &str,
        arguments: &str,
        cancel: &CancellationToken,
    ) -> Result<CallOutput, Error> {
        let arguments: &RawValue = borrow_arguments(arguments)?;
        let text = match &self.transport {
            Transport::Stdio(process) => process.call(self.era, name, arguments, cancel).await?,
            Transport::Http(server) => self.http_call(server, name, arguments, cancel).await?,
        };
        output(&text)
    }

    /// The protocol era this server was found to speak.
    pub fn era(&self) -> Era {
        self.era
    }

    /// The last few warnings, such as a tool dropped for a bad `x-mcp-header`.
    pub fn warnings(&self) -> Vec<String> {
        match &self.transport {
            Transport::Stdio(_) => Vec::new(),
            Transport::Http(server) => server.warnings(),
        }
    }

    /// Ends the server: the process group, or the HTTP session.
    pub async fn stop(self) {
        match self.transport {
            Transport::Stdio(mut process) => {
                if process.reaped() {
                    process.group.disarm();
                } else {
                    process.group.stop().await;
                }
                // A server that keeps its pipes open never holds the stop.
                let _ = tokio::time::timeout(STOP_LIMIT, &mut process.reader).await;
            }
            Transport::Http(server) => server.stop().await,
        }
    }

    /// One request and its result text, in the era of this server.
    async fn request(&self, method: &str, params: Params<'_>) -> Result<Bytes, Error> {
        match &self.transport {
            Transport::Stdio(process) => process.wire.request(self.era, method, params).await,
            Transport::Http(server) => server.request(self.era, method, params).await,
        }
    }

    /// Whether the server announced a new tool list.
    fn stale(&self) -> bool {
        match &self.transport {
            Transport::Stdio(process) => process.wire.stale.load(Ordering::Relaxed),
            Transport::Http(server) => server.stale(),
        }
    }

    fn set_stale(&self, stale: bool) {
        match &self.transport {
            Transport::Stdio(process) => process.wire.stale.store(stale, Ordering::Relaxed),
            Transport::Http(server) => server.set_stale(stale),
        }
    }

    /// Calls one tool over HTTP, with its `x-mcp-header` arguments as headers.
    async fn http_call(
        &self,
        server: &http::Http,
        name: &str,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> Result<Bytes, Error> {
        let mut retried = false;
        loop {
            let extra = self.call_headers(name, arguments);
            let id = server.next_id();
            let params = Params::Call { name, arguments };
            let sent = server.call(id, self.era, params, extra);
            let reply = tokio::select! {
                reply = sent => reply,
                () = cancel.cancelled() => {
                    // Dropping the stream is the cancel signal; a legacy server
                    // also wants the notification.
                    if matches!(self.era, Era::Legacy(_)) {
                        server.cancel(self.era, id);
                    }
                    return Err(Error::Cancelled);
                }
            };
            match reply {
                Err(Error::Rpc { code, .. }) if code == protocol::HEADER_MISMATCH && !retried => {
                    retried = true;
                    *self.listed_write() = None;
                    self.tools().await?;
                }
                other => return other,
            }
        }
    }

    /// The headers a call sends for the `x-mcp-header` arguments of its tool.
    fn call_headers(&self, name: &str, arguments: &RawValue) -> Vec<(HeaderName, HeaderValue)> {
        let params = self
            .listed_read()
            .as_ref()
            .and_then(|listed| listed.headers(name));
        match params {
            Some(params) => http::call_headers(&params, arguments.get()),
            None => Vec::new(),
        }
    }

    /// Every page of the tool list, with the ttl of the first.
    async fn list(&self) -> Result<(Vec<Tool>, Option<Duration>), Error> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut ttl = None;
        loop {
            let params = Params::List {
                cursor: cursor.as_deref(),
            };
            let text = self.request("tools/list", params).await?;
            let page: Page = serde_json::from_slice(&text).map_err(protocol_error)?;
            complete(page.result_type)?;
            if ttl.is_none() {
                ttl = page.ttl_ms.map(Duration::from_millis);
            }
            tools.extend(page.tools.into_iter().filter_map(|entry| self.tool(entry)));
            match page.next_cursor {
                Some(next) => cursor = Some(next.into_owned()),
                None => break,
            }
        }
        Ok((tools, ttl))
    }

    /// One tool of a page, or `None` when its headers cannot be used.
    fn tool(&self, entry: Entry<'_>) -> Option<Tool> {
        let mut tool = entry.tool();
        if let Transport::Http(server) = &self.transport {
            match http::param_headers(&tool.input_schema) {
                Ok(headers) => tool.headers = headers.into(),
                Err(reason) => {
                    server.warn(format!("dropped the tool {}: {reason}", tool.name));
                    return None;
                }
            }
        }
        Some(tool)
    }
}

impl Process {
    /// Calls one tool and returns the result text.
    async fn call(
        &self,
        era: Era,
        name: &str,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> Result<Bytes, Error> {
        let params = Params::Call { name, arguments };
        let (id, reply) = self.wire.start(era, "tools/call", params)?;
        tokio::select! {
            received = reply => match received {
                Ok(Reply::Result(text)) => Ok(text),
                Ok(Reply::Error(failure)) => Err(failure.error()),
                Err(_) => Err(self.wire.exited()),
            },
            () = cancel.cancelled() => {
                self.wire.forget(id);
                let cancelled = Params::Cancelled { request_id: id };
                let _ = self
                    .wire
                    .notify(era, "notifications/cancelled", cancelled);
                Err(Error::Cancelled)
            }
        }
    }
}

/// What the server answered, before the era is known.
async fn detect(wire: &Wire) -> Result<Era, Error> {
    let probe = wire
        .reply(
            Era::Modern(protocol::MODERN),
            "server/discover",
            Params::Discover,
            Some(DISCOVER_TIMEOUT),
        )
        .await;
    match probe {
        Ok(Reply::Result(_)) => Ok(Era::Modern(protocol::MODERN)),
        Ok(Reply::Error(failure)) if failure.code == protocol::UNSUPPORTED_VERSION => {
            version_shared(&failure)
        }
        _ => handshake(wire).await,
    }
}

/// The legacy handshake: `initialize`, then `notifications/initialized`.
async fn handshake(wire: &Wire) -> Result<Era, Error> {
    let asked = Era::Legacy(protocol::LEGACY_DEFAULT);
    let params = Params::Initialize {
        version: protocol::LEGACY_DEFAULT,
    };
    let text = wire.request(asked, "initialize", params).await?;
    let version = legacy_version(&text)?;
    wire.notify(
        Era::Legacy(version),
        "notifications/initialized",
        Params::Empty,
    )?;
    Ok(Era::Legacy(version))
}

/// The version a legacy `initialize` result names.
pub(crate) fn legacy_version(text: &[u8]) -> Result<&'static str, Error> {
    let answer: Handshake = serde_json::from_slice(text).map_err(protocol_error)?;
    let Some(name) = answer.protocol_version else {
        return Err(Error::Version {
            server: "no protocolVersion".to_owned(),
            client: protocol::CLIENT_VERSIONS,
        });
    };
    protocol::legacy_version(name).ok_or_else(|| Error::Version {
        server: name.to_owned(),
        client: protocol::CLIENT_VERSIONS,
    })
}

/// The era a `-32022` answer names: the version both sides speak.
pub(crate) fn version_shared(failure: &RpcFailure) -> Result<Era, Error> {
    match shared_version(failure) {
        Some(version) => Ok(Era::Modern(version)),
        None => Err(Error::Version {
            server: failure.supported_text(),
            client: protocol::CLIENT_VERSIONS,
        }),
    }
}

/// The modern version out of the ones a `-32022` error listed.
fn shared_version(failure: &RpcFailure) -> Option<&'static str> {
    let data = failure.data.as_deref()?;
    let supported: protocol::Supported = serde_json::from_str(data).ok()?;
    supported
        .supported
        .into_iter()
        .find(|version| *version == protocol::MODERN)
        .map(|_| protocol::MODERN)
}

/// The tool result as the model sees it: one text, one structured value.
fn output(text: &[u8]) -> Result<CallOutput, Error> {
    let result: CallResult = serde_json::from_slice(text).map_err(protocol_error)?;
    complete(result.result_type)?;
    let mut joined = String::with_capacity(
        result.content.len()
            + result
                .content
                .iter()
                .map(|item| item.text.as_ref().map_or(0, |text| text.len()))
                .sum::<usize>(),
    );
    for (index, item) in result.content.iter().enumerate() {
        if index > 0 {
            joined.push('\n');
        }
        match (item.kind.as_deref(), item.text.as_deref()) {
            (Some("text"), Some(text)) => joined.push_str(text),
            (Some("text"), None) => {}
            (kind, _) => {
                let _ = write!(joined, "[{} content]", kind.unwrap_or("unknown"));
            }
        }
    }
    Ok(CallOutput {
        text: joined,
        structured: result.structured,
        is_error: result.is_error,
    })
}

/// Refuses any result type but a complete one, which is all this client reads.
fn complete(result_type: Option<&str>) -> Result<(), Error> {
    match result_type {
        None | Some("complete") => Ok(()),
        Some(other) => Err(Error::Protocol(format!(
            "the result type {other} is not one this client supports"
        ))),
    }
}

fn protocol_error(error: serde_json::Error) -> Error {
    Error::Protocol(error.to_string())
}

/// The model's arguments as JSON text, borrowed rather than reparsed.
fn borrow_arguments(arguments: &str) -> Result<&RawValue, Error> {
    let text = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(text).map_err(|error| Error::Arguments(error.to_string()))
}

/// One tool list of one server, with what a call reads out of it.
struct Listed {
    /// The tools, in the server's order.
    tools: Arc<[Tool]>,
    /// Where each tool that sends header arguments sits in `tools`, by name.
    by_name: HashMap<Box<str>, usize>,
    /// When the server's ttl says the list lapses.
    expires: Option<Instant>,
}

impl Listed {
    /// The list of `tools`, which lapses at `ttl` when the server names one.
    fn new(tools: Vec<Tool>, ttl: Option<Duration>) -> Listed {
        let mut by_name = HashMap::new();
        for (at, tool) in tools.iter().enumerate() {
            if !tool.headers.is_empty() {
                by_name.entry(Box::from(tool.name.as_str())).or_insert(at);
            }
        }
        Listed {
            tools: tools.into(),
            by_name,
            expires: ttl.map(|ttl| Instant::now() + ttl),
        }
    }

    /// The tools, when the ttl has not lapsed.
    fn fresh(&self) -> Option<Arc<[Tool]>> {
        if self.expires.is_some_and(|at| Instant::now() >= at) {
            return None;
        }
        Some(Arc::clone(&self.tools))
    }

    /// The header arguments of the tool `name`, when it sends any.
    fn headers(&self, name: &str) -> Option<Arc<[HeaderParam]>> {
        let at = *self.by_name.get(name)?;
        Some(Arc::clone(&self.tools[at].headers))
    }
}

/// Marks the list of a client stale when a relist ends without storing one.
struct Restale<'a>(Option<&'a Client>);

impl Drop for Restale<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.0 {
            client.set_stale(true);
        }
    }
}

/// The frames and shared state of one server, held by the client and the tasks.
struct Wire {
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    /// Set when the server says its tool list changed.
    stale: AtomicBool,
    state: Mutex<Served>,
}

impl Wire {
    fn start(
        &self,
        era: Era,
        method: &str,
        params: Params<'_>,
    ) -> Result<(u64, oneshot::Receiver<Reply>), Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending().insert(id, sender);
        // Checked after the insert: a server that ended drops every sender, so
        // a call that lands after it fails here instead of waiting for ever.
        if self.served().gone.is_some() {
            self.forget(id);
            return Err(self.exited());
        }
        if let Err(error) = self.send(protocol::frame(Some(id), method, params, era)) {
            self.forget(id);
            return Err(error);
        }
        Ok((id, receiver))
    }

    async fn request(&self, era: Era, method: &str, params: Params<'_>) -> Result<Bytes, Error> {
        match self.reply(era, method, params, None).await? {
            Reply::Result(text) => Ok(text),
            Reply::Error(failure) => Err(failure.error()),
        }
    }

    async fn reply(
        &self,
        era: Era,
        method: &str,
        params: Params<'_>,
        limit: Option<Duration>,
    ) -> Result<Reply, Error> {
        let (id, receiver) = self.start(era, method, params)?;
        let received = match limit {
            Some(limit) => match tokio::time::timeout(limit, receiver).await {
                Ok(received) => received,
                Err(_) => {
                    self.forget(id);
                    return Err(Error::Timeout {
                        method: method.to_owned(),
                        ms: limit.as_millis() as u64,
                    });
                }
            },
            None => receiver.await,
        };
        received.map_err(|_| self.exited())
    }

    fn notify(&self, era: Era, method: &str, params: Params<'_>) -> Result<(), Error> {
        self.send(protocol::frame(None, method, params, era))
    }

    fn send(&self, frame: Vec<u8>) -> Result<(), Error> {
        self.out.send(frame).map_err(|_| self.exited())
    }

    fn forget(&self, id: u64) {
        self.pending().remove(&id);
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Reply>>> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn exited(&self) -> Error {
        Error::Exited {
            reason: self.served().reason(),
        }
    }

    fn served(&self) -> MutexGuard<'_, Served> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Records why the server is gone and lets every waiter fail.
    fn finish(&self, note: String) {
        {
            let mut state = self.served();
            state.gone.get_or_insert(note);
        }
        self.pending().clear();
    }

    /// Routes one line from the server: a request, a notification or a reply.
    fn route(&self, line: &Bytes) {
        let Ok(message) = serde_json::from_slice::<protocol::Message<'_>>(line) else {
            return;
        };
        match (message.method, message.id) {
            (Some(method), Some(id)) => {
                let frame = match method {
                    "ping" => protocol::result(id, "{}"),
                    other => protocol::error(
                        id,
                        protocol::METHOD_NOT_FOUND,
                        &format!("method not found: {other}"),
                    ),
                };
                let _ = self.send(frame);
            }
            (Some(method), None) => {
                if method == protocol::LIST_CHANGED {
                    self.stale.store(true, Ordering::Relaxed);
                }
            }
            (None, Some(id)) => self.deliver(id, message.result, message.error, line),
            (None, None) => {}
        }
    }

    /// Hands a response to the caller that waits for its id.
    fn deliver(
        &self,
        id: &RawValue,
        result: Option<&RawValue>,
        error: Option<protocol::ErrorObject<'_>>,
        line: &Bytes,
    ) {
        let Ok(id) = id.get().parse::<u64>() else {
            return;
        };
        let Some(sender) = self.pending().remove(&id) else {
            return;
        };
        if let Some(reply) = response(result, error, Some(line)) {
            let _ = sender.send(reply);
        }
    }
}

/// The reply a response frame carries, `None` when it carries neither.
///
/// `body` is the buffer `result` was parsed from, when the result can be a
/// slice of it instead of a copy.
pub(crate) fn response(
    result: Option<&RawValue>,
    error: Option<protocol::ErrorObject<'_>>,
    body: Option<&Bytes>,
) -> Option<Reply> {
    match (result, error) {
        (Some(result), _) => Some(Reply::Result(match body {
            Some(body) => part_of(body, result),
            None => Bytes::copy_from_slice(result.get().as_bytes()),
        })),
        (None, Some(error)) => Some(Reply::Error(RpcFailure {
            code: error.code,
            message: error.message.into_owned(),
            data: error.data.map(|data| data.get().into()),
        })),
        (None, None) => None,
    }
}

/// The bytes of `body` that `value` is, without copying them when the two
/// share one buffer.
fn part_of(body: &Bytes, value: &RawValue) -> Bytes {
    let text = value.get();
    let start = (text.as_ptr() as usize).checked_sub(body.as_ptr() as usize);
    match start {
        Some(start) if start + text.len() <= body.len() => body.slice(start..start + text.len()),
        _ => Bytes::copy_from_slice(text.as_bytes()),
    }
}

/// What the server said on stderr, and why it is gone.
#[derive(Default)]
struct Served {
    tail: Vec<u8>,
    gone: Option<String>,
}

impl Served {
    fn push(&mut self, bytes: &[u8]) {
        self.tail.extend_from_slice(bytes);
        let over = self.tail.len().saturating_sub(STDERR_TAIL);
        if over > 0 {
            self.tail.drain(..over);
        }
    }

    fn reason(&self) -> String {
        let mut reason = self
            .gone
            .clone()
            .unwrap_or_else(|| "no answer from the server".to_owned());
        let tail = String::from_utf8_lossy(&self.tail);
        let tail = tail.trim_end();
        if !tail.is_empty() {
            reason.push_str("; stderr: ");
            reason.push_str(tail);
        }
        reason
    }
}

/// The answer to one request.
pub(crate) enum Reply {
    Result(Bytes),
    Error(RpcFailure),
}

/// A JSON-RPC error from the server.
pub(crate) struct RpcFailure {
    pub(crate) code: i64,
    pub(crate) message: String,
    pub(crate) data: Option<Box<str>>,
}

impl RpcFailure {
    pub(crate) fn error(self) -> Error {
        Error::Rpc {
            code: self.code,
            message: self.message,
        }
    }

    /// The versions a `-32022` error listed, for the message about them.
    pub(crate) fn supported_text(&self) -> String {
        let listed = self
            .data
            .as_deref()
            .and_then(|data| serde_json::from_str::<protocol::Supported>(data).ok());
        match listed {
            Some(supported) if !supported.supported.is_empty() => supported.supported.join(", "),
            _ => "a version this client does not know".to_owned(),
        }
    }
}

/// One page of `tools/list`.
#[derive(Deserialize)]
struct Page<'a> {
    #[serde(borrow, default)]
    tools: Vec<Entry<'a>>,
    #[serde(borrow, rename = "nextCursor", default)]
    next_cursor: Option<Cow<'a, str>>,
    #[serde(rename = "ttlMs", default)]
    ttl_ms: Option<u64>,
    #[serde(borrow, rename = "resultType", default)]
    result_type: Option<&'a str>,
}

/// One tool of a page.
#[derive(Deserialize)]
struct Entry<'a> {
    #[serde(borrow)]
    name: &'a str,
    #[serde(borrow, default)]
    description: Option<Cow<'a, str>>,
    #[serde(rename = "inputSchema", default)]
    input_schema: Option<Value>,
    #[serde(rename = "outputSchema", default)]
    output_schema: Option<Value>,
    #[serde(default)]
    annotations: Option<Value>,
}

impl Entry<'_> {
    fn tool(self) -> Tool {
        Tool {
            name: self.name.to_owned(),
            description: self.description.map(Cow::into_owned),
            input_schema: self
                .input_schema
                .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
            output_schema: self.output_schema,
            annotations: self.annotations,
            headers: Vec::new().into(),
        }
    }
}

/// The result of `tools/call`.
#[derive(Deserialize)]
struct CallResult<'a> {
    #[serde(borrow, default)]
    content: Vec<Content<'a>>,
    #[serde(rename = "structuredContent", default)]
    structured: Option<Value>,
    #[serde(rename = "isError", default)]
    is_error: bool,
    #[serde(borrow, rename = "resultType", default)]
    result_type: Option<&'a str>,
}

/// One item of a tool result.
#[derive(Deserialize)]
struct Content<'a> {
    #[serde(borrow, rename = "type", default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
}

/// The result of the legacy `initialize`.
#[derive(Deserialize)]
struct Handshake<'a> {
    #[serde(borrow, rename = "protocolVersion", default)]
    protocol_version: Option<&'a str>,
}

/// Writes the frames of every caller, joining those that are ready together.
async fn write_frames(
    mut stdin: ChildStdin,
    mut frames: mpsc::UnboundedReceiver<Vec<u8>>,
    wire: Arc<Wire>,
) {
    let mut batch = Vec::with_capacity(WRITE_BATCH);
    let mut joined = Vec::new();
    loop {
        batch.clear();
        if frames.recv_many(&mut batch, WRITE_BATCH).await == 0 {
            break;
        }
        // A lone frame is written as it is; only a batch is joined.
        let bytes = match batch.as_slice() {
            [frame] => frame.as_slice(),
            frames => {
                joined.clear();
                for frame in frames {
                    joined.extend_from_slice(frame);
                }
                joined.as_slice()
            }
        };
        if stdin.write_all(bytes).await.is_err() || stdin.flush().await.is_err() {
            // Nobody reads the frames any more: the calls that wait fail now.
            wire.finish("the server closed its input".to_owned());
            break;
        }
    }
}

/// Keeps the last of the server's stderr for error text.
async fn drain_stderr(mut stderr: ChildStderr, wire: Arc<Wire>) {
    let mut buffer = vec![0_u8; STDERR_TAIL];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => wire.served().push(&buffer[..read]),
        }
    }
}

/// Reads the server's lines until it ends, then reaps it.
async fn read_lines(mut child: Child, stdout: ChildStdout, wire: Arc<Wire>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut failed = None;
    loop {
        match read_line(&mut reader, &mut line).await {
            Ok(true) => wire.route(&Bytes::from(std::mem::take(&mut line))),
            Ok(false) => break,
            Err(error) => {
                failed = Some(error.to_string());
                break;
            }
        }
    }
    let note = match failed {
        Some(reason) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            reason
        }
        None => match child.wait().await {
            Ok(status) => format!("the server ended with {status}"),
            Err(error) => format!("the server could not be waited for: {error}"),
        },
    };
    wire.finish(note);
}

/// The next line of the server; false at its end, an error past the read cap.
async fn read_line(reader: &mut BufReader<ChildStdout>, line: &mut Vec<u8>) -> Result<bool, Error> {
    line.clear();
    loop {
        let (read, newline) = {
            let available = reader.fill_buf().await.map_err(|error| {
                Error::Protocol(format!("the server's output could not be read: {error}"))
            })?;
            if available.is_empty() {
                return Ok(!line.is_empty());
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(end) => {
                    line.extend_from_slice(&available[..end]);
                    (end + 1, true)
                }
                None => {
                    line.extend_from_slice(available);
                    (available.len(), false)
                }
            }
        };
        reader.consume(read);
        if newline {
            return Ok(true);
        }
        if line.len() > READ_CAP {
            return Err(Error::Protocol(format!(
                "the server sent a line over the {READ_CAP_MIB} MiB cap this client reads"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_after_the_server_ended_fails_at_once() {
        let (out, frames) = mpsc::unbounded_channel();
        let wire = Wire {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            stale: AtomicBool::new(false),
            state: Mutex::new(Served::default()),
        };
        wire.finish("the server ended".to_owned());

        let error = wire
            .start(
                Era::Legacy(protocol::LEGACY_DEFAULT),
                "tools/call",
                Params::Empty,
            )
            .err()
            .expect("a call after the end fails");
        assert!(error.to_string().contains("is gone"), "{error}");
        assert!(frames.is_empty(), "a frame was written to a gone server");
    }
}
