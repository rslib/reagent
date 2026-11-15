//! End-to-end tests of `reagent acp`, the ACP v1 agent an editor runs over
//! stdin and stdout.
//!
//! Each test starts the binary as the editor would, against a scripted fake
//! provider and a state folder of its own, so these tests exist only with the
//! `testing` feature.
#![cfg(feature = "testing")]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for a line before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// A state folder short enough for a socket path, which the platform caps.
fn state() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ra")
        .tempdir_in("/tmp")
        .expect("a temp folder")
}

/// A folder the session runs in, with the repository marker the config walks up
/// to.
async fn work(state: &Path) -> PathBuf {
    let cwd = state.join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    tokio::fs::create_dir(cwd.join(".git"))
        .await
        .expect("the repo folder");
    cwd
}

/// A folder whose project config asks a person before every `bash` call.
async fn ask_folder(cwd: &Path) {
    tokio::fs::create_dir_all(cwd.join(".reagent"))
        .await
        .expect("the config folder");
    tokio::fs::write(
        cwd.join(".reagent").join("config.toml"),
        "[permissions]\nask = [\"Bash\"]\n",
    )
    .await
    .expect("the rules");
}

/// A scripted provider reply, and the `--provider` value that reads it.
async fn script(dir: &Path, body: &str) -> String {
    let path = dir.join("script.json");
    tokio::fs::write(&path, body)
        .await
        .expect("writes the script");
    format!("fake:{}", path.display())
}

/// The agent this test drives: its stdin, its stdout and the stderr it wrote.
struct Acp {
    /// Held so that `kill_on_drop` ends the process when the test returns.
    #[allow(dead_code, reason = "the drop of this struct kills the process")]
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<String>>,
    /// The id of the next request this test sends.
    next: u64,
    /// The lines read but not used yet, oldest first.
    pending: VecDeque<Value>,
}

impl Acp {
    /// Starts `reagent acp` in `cwd` with `args`, under `state`.
    async fn start(state: &Path, cwd: &Path, args: &[&str]) -> Acp {
        let mut child = Command::new(BIN)
            .env("XDG_STATE_HOME", state)
            .env("XDG_CONFIG_HOME", state.join("config"))
            .env("XDG_CACHE_HOME", state.join("cache"))
            .arg("acp")
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the agent starts");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let text = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(read) = pipe.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    if let Ok(mut held) = text.lock() {
                        held.push_str(&String::from_utf8_lossy(&chunk[..read]));
                    }
                }
            });
        }
        Acp {
            child,
            stdin,
            stdout,
            stderr,
            next: 0,
            pending: VecDeque::new(),
        }
    }

    /// The stderr the agent wrote so far.
    fn said(&self) -> String {
        self.stderr
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    /// Writes one message as one line.
    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).expect("JSON");
        line.push(b'\n');
        self.stdin.write_all(&line).await.expect("writes the line");
    }

    /// Sends one request and answers the id it carries.
    async fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    /// Sends one notification.
    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// The next line of stdout, as JSON.
    async fn line(&mut self) -> Value {
        let mut text = String::new();
        let read = tokio::time::timeout(WAIT, self.stdout.read_line(&mut text))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "no line in {WAIT:?}; read {:?}; stderr: {}",
                    self.pending,
                    self.said()
                )
            })
            .expect("the agent's stdout reads");
        assert!(
            read > 0,
            "the agent closed its stdout; read {:?}; stderr: {}",
            self.pending,
            self.said()
        );
        serde_json::from_str(text.trim_end())
            .unwrap_or_else(|error| panic!("{error}: {text:?}; stderr: {}", self.said()))
    }

    /// The lines up to the first one `stop` holds for, that one last.
    async fn until(&mut self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut taken = Vec::new();
        loop {
            if let Some(at) = self.pending.iter().position(&stop) {
                taken.extend(self.pending.drain(..=at));
                return taken;
            }
            let line = self.line().await;
            self.pending.push_back(line);
        }
    }

    /// The answer to the request `id`, which keeps the lines read before it.
    async fn answer(&mut self, id: u64) -> Value {
        let mut lines = VecDeque::from(self.until(|line| line["id"].as_u64() == Some(id)).await);
        let answer = lines.pop_back().expect("an answer");
        while let Some(line) = lines.pop_back() {
            self.pending.push_front(line);
        }
        answer
    }

    /// Initializes the connection, as an editor does first.
    async fn initialize(&mut self) -> Value {
        let id = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {"name": "acp-test", "version": "1"},
                }),
            )
            .await;
        self.answer(id).await
    }

    /// Starts a session in `cwd` and answers its id.
    async fn new_session(&mut self, cwd: &Path) -> String {
        self.new_session_with(cwd, json!([])).await
    }

    /// Starts a session in `cwd` with `servers` in `mcpServers`.
    async fn new_session_with(&mut self, cwd: &Path, servers: Value) -> String {
        let id = self
            .request(
                "session/new",
                json!({"cwd": path(cwd), "mcpServers": servers}),
            )
            .await;
        let answer = self.answer(id).await;
        answer["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("a session id: {answer}; stderr: {}", self.said()))
            .to_string()
    }

    /// Sends one prompt of one text block and answers the id of its request.
    async fn prompt(&mut self, session: &str, text: &str) -> u64 {
        self.request(
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": text}],
            }),
        )
        .await
    }
}

/// A path as a JSON string.
fn path(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}

/// The `update` of each `session/update` line of `lines`.
fn updates(lines: &[Value]) -> Vec<&Value> {
    lines
        .iter()
        .filter(|line| line["method"] == "session/update")
        .map(|line| &line["params"]["update"])
        .collect()
}

/// The text of the update chunks of `kind` in `lines`, joined.
fn chunks(lines: &[Value], kind: &str) -> String {
    let mut text = String::new();
    for update in updates(lines) {
        if update["sessionUpdate"] == kind {
            text.push_str(update["content"]["text"].as_str().unwrap_or_default());
        }
    }
    text
}

/// The first update of `kind` in `lines`.
fn update_of<'a>(lines: &'a [Value], kind: &str) -> &'a Value {
    updates(lines)
        .into_iter()
        .find(|update| update["sessionUpdate"] == kind)
        .unwrap_or_else(|| panic!("no {kind} in {lines:?}"))
}

/// `initialize` answers protocol version one, its capabilities and its name,
/// and `authenticate` is answered with no work to do.
#[tokio::test]
async fn acp_initialize_answers_version_one() {
    let state = state();
    let cwd = work(state.path()).await;
    let mut acp = Acp::start(state.path(), &cwd, &[]).await;

    let answer = acp.initialize().await;
    let result = &answer["result"];
    assert_eq!(result["protocolVersion"], 1, "{answer}");
    let agent = &result["agentCapabilities"];
    assert_eq!(agent["loadSession"], true, "{answer}");
    assert_eq!(agent["promptCapabilities"]["image"], false, "{answer}");
    assert_eq!(agent["promptCapabilities"]["audio"], false, "{answer}");
    assert_eq!(
        agent["promptCapabilities"]["embeddedContext"], true,
        "{answer}"
    );
    assert_eq!(agent["mcpCapabilities"]["http"], true, "{answer}");
    assert_eq!(agent["mcpCapabilities"]["sse"], false, "{answer}");
    assert_eq!(result["agentInfo"]["name"], "reagent", "{answer}");
    assert!(
        result["agentInfo"]["version"].is_string(),
        "the version is named: {answer}"
    );
    assert_eq!(result["authMethods"], json!([]), "{answer}");

    let id = acp
        .request("authenticate", json!({"methodId": "none"}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"], json!({}), "{answer}");
}

/// One prompt streams the reply as update chunks and ends the turn.
#[tokio::test]
async fn acp_prompt_streams_text_and_ends_the_turn() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"hello from reagent"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "say hi").await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(
        chunks(&lines, "agent_message_chunk").contains("hello from reagent"),
        "the reply is streamed: {lines:?}"
    );
    for line in &lines[..lines.len() - 1] {
        assert_eq!(line["method"], "session/update", "{line}");
        assert_eq!(line["params"]["sessionId"], session.as_str(), "{line}");
    }
}

/// A tool call is reported when it starts and again with its result.
#[tokio::test]
async fn acp_tool_call_reports_start_and_result() {
    let state = state();
    let cwd = work(state.path()).await;
    tokio::fs::write(cwd.join("a.txt"), "hi there\n")
        .await
        .expect("the file");
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"read","arguments":{"path":"a.txt"}}]},{"text":"done"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "read it").await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "end_turn",
        "{:?}",
        lines.last()
    );

    let call = update_of(&lines, "tool_call");
    assert_eq!(call["toolCallId"], "call_0", "{call}");
    assert_eq!(call["title"], "read", "{call}");
    assert_eq!(call["kind"], "read", "{call}");
    assert_eq!(call["status"], "pending", "{call}");

    let ready = updates(&lines)
        .into_iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("a tool call update");
    assert_eq!(ready["toolCallId"], "call_0", "{ready}");
    assert_eq!(ready["status"], "in_progress", "{ready}");
    assert_eq!(ready["rawInput"]["path"], "a.txt", "{ready}");
    assert_eq!(ready["title"], "Read a.txt", "{ready}");

    let done = updates(&lines)
        .into_iter()
        .rev()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("a last update");
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["content"][0]["type"], "content", "{done}");
    assert!(
        done["content"][0]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("hi there")),
        "the output is shown: {done}"
    );
}

/// An approval is asked for in ACP terms, and both answers reach the worker.
#[tokio::test]
async fn acp_permission_round_trip() {
    let state = state();
    let cwd = work(state.path()).await;
    ask_folder(&cwd).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"bash","arguments":{"command":"echo hi"}}]},{"text":"done"}]}"#,
    )
    .await;

    // An `allow_once` answer runs the call.
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    let id = acp.prompt(&session, "run it").await;
    let asked = acp
        .until(|line| line["method"] == "session/request_permission")
        .await;
    let request = asked.last().expect("a permission request");
    let options = request["params"]["options"]
        .as_array()
        .unwrap_or_else(|| panic!("options: {request}"));
    let kinds: Vec<&str> = options
        .iter()
        .map(|option| option["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(
        kinds,
        ["allow_once", "allow_always", "reject_once"],
        "{request}"
    );
    assert_eq!(
        request["params"]["toolCall"]["toolCallId"], "call_0",
        "{request}"
    );
    assert_eq!(
        request["params"]["sessionId"],
        session.as_str(),
        "{request}"
    );
    let ask = request["id"].as_u64().expect("a request id");
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": ask,
        "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}},
    }))
    .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "end_turn",
        "{:?}",
        lines.last()
    );
    let done = updates(&lines)
        .into_iter()
        .rev()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("a last update");
    assert_eq!(done["status"], "completed", "{done}");

    // A `reject_once` answer refuses it, and the run goes on.
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"bash","arguments":{"command":"echo hi"}}]},{"text":"done"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    let id = acp.prompt(&session, "run it").await;
    let asked = acp
        .until(|line| line["method"] == "session/request_permission")
        .await;
    let ask = asked.last().expect("a permission request")["id"]
        .as_u64()
        .expect("a request id");
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": ask,
        "result": {"outcome": {"outcome": "selected", "optionId": "reject_once"}},
    }))
    .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "end_turn",
        "{:?}",
        lines.last()
    );
    let done = updates(&lines)
        .into_iter()
        .rev()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("a last update");
    assert_eq!(done["status"], "failed", "{done}");
}

/// A cancel while a run waits for an approval answers the prompt `cancelled`.
#[tokio::test]
async fn acp_cancel_ends_with_cancelled() {
    let state = state();
    let cwd = work(state.path()).await;
    ask_folder(&cwd).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"bash","arguments":{"command":"echo hi"}}]},{"text":"done"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "run it").await;
    acp.until(|line| line["method"] == "session/request_permission")
        .await;
    acp.notify("session/cancel", json!({"sessionId": session}))
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "cancelled",
        "{:?}",
        lines.last()
    );
}

/// A second process replays the conversation of a session before it answers.
#[tokio::test]
async fn acp_load_replays_the_history() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"hello from reagent"}]}"#,
    )
    .await;
    let mut first = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    first.initialize().await;
    let session = first.new_session(&cwd).await;
    let id = first.prompt(&session, "say hi").await;
    first.until(|line| line["id"].as_u64() == Some(id)).await;

    let mut second = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    second.initialize().await;
    let id = second
        .request(
            "session/load",
            json!({"sessionId": session, "cwd": path(&cwd), "mcpServers": []}),
        )
        .await;
    let lines = second.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"], json!({}), "{answer}");
    assert!(
        chunks(&lines, "user_message_chunk").contains("say hi"),
        "the prompt is replayed: {lines:?}"
    );
    assert!(
        chunks(&lines, "agent_message_chunk").contains("hello from reagent"),
        "the reply is replayed: {lines:?}"
    );
}

/// The MCP servers a session is given reach the worker it starts.
#[tokio::test]
async fn acp_mcp_servers_reach_the_worker() {
    let state = state();
    let cwd = work(state.path()).await;
    let marker = state.path().join("mcp-reached");
    let provider = script(state.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let _session = acp
        .new_session_with(
            &cwd,
            json!([{
                "name": "acptest",
                "command": "/bin/sh",
                "args": ["-c", format!("echo reached > {}", marker.display())],
            }]),
        )
        .await;
    for _ in 0..300 {
        if marker.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "the worker never started the MCP server of the session; stderr: {}",
        acp.said()
    );
}

/// A `cwd` that is not an existing folder is refused.
#[tokio::test]
async fn acp_bad_cwd_is_invalid_params() {
    let state = state();
    let cwd = work(state.path()).await;
    let mut acp = Acp::start(state.path(), &cwd, &[]).await;
    acp.initialize().await;
    let id = acp
        .request(
            "session/new",
            json!({"cwd": "/nonexistent/reagent-acp-missing", "mcpServers": []}),
        )
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32602, "invalid params: {answer}");
}

/// An image block is refused, since this agent takes no images.
#[tokio::test]
async fn acp_image_block_is_refused() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"never asked"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    let id = acp
        .request(
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}],
            }),
        )
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32602, "invalid params: {answer}");
}
