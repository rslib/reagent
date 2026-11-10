//! End-to-end tests of a steered `-p` run: the requests it takes on stdin and
//! on its socket reach the run while it goes on.
//!
//! Each test starts the binary against a scripted provider and a state folder
//! of its own, so these tests exist only with the `testing` feature.
#![cfg(feature = "testing")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for a line or an exit before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// How long a test waits for a run that must end on its own to leave.
const SOON: Duration = Duration::from_secs(10);

/// How many bytes one read of a test client asks for.
const CHUNK: usize = 8192;

/// A state folder short enough for a socket path, which the platform caps.
fn state() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rs")
        .tempdir_in("/tmp")
        .expect("a temp folder")
}

/// A scripted provider reply, and the `--provider` value that reads it.
async fn script(dir: &Path, body: &str) -> String {
    let path = dir.join("script.json");
    tokio::fs::write(&path, body)
        .await
        .expect("writes the script");
    format!("fake:{}", path.display())
}

/// A turn that calls `bash` with `command`, then a reply that ends the run: the
/// first call is the slow one a test steers into.
fn slow_script(command: &str) -> String {
    serde_json::json!({
        "replies": [
            {"calls": [{"name": "bash", "arguments": {"command": command}}]},
            {"text": "done"}
        ]
    })
    .to_string()
}

/// A turn that calls `bash` with `command`, then a reply that ends the run.
fn asked_script(command: &str) -> String {
    slow_script(command)
}

/// A folder whose project config asks a person before every `bash` call.
async fn ask_folder(cwd: &Path) {
    tokio::fs::create_dir_all(cwd.join(".reagent"))
        .await
        .expect("the config folder");
    tokio::fs::create_dir(cwd.join(".git"))
        .await
        .expect("the repo folder");
    tokio::fs::write(
        cwd.join(".reagent").join("config.toml"),
        "[permissions]\nask = [\"Bash\"]\n",
    )
    .await
    .expect("the rules");
}

/// A reply that ends the turn with one short text, as the Anthropic wire
/// streams it.
fn http_reply(text: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n"
        ),
        serde_json::to_string(text).expect("a JSON string")
    )
}

/// A reply that calls `bash` with `command` and ends the turn with the tool
/// use.
fn call_reply(name: &str, arguments: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"{}\"}}}}\n\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n"
        ),
        name,
        serde_json::to_string(arguments).expect("a JSON string")
    )
}

/// A fake Anthropic server: it answers `replies` in order and records the body
/// of each request it gets.
struct Server {
    base: String,
    bodies: mpsc::UnboundedReceiver<String>,
}

/// Starts a server that answers `replies` in order.
async fn serve(replies: Vec<String>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    let (sender, bodies) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let Some(body) = read_body(&mut socket).await else {
                return;
            };
            let _ = sender.send(body);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    Server { base, bodies }
}

impl Server {
    /// The next recorded request body.
    async fn body(&mut self) -> String {
        tokio::time::timeout(WAIT, self.bodies.recv())
            .await
            .expect("a recorded request in time")
            .expect("a recorded request")
    }
}

/// Reads one HTTP request and returns its body.
async fn read_body(socket: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim() == "content-length")
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= end + 4 + length {
                return Some(
                    String::from_utf8_lossy(&buffer[end + 4..end + 4 + length]).into_owned(),
                );
            }
        }
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// A folder whose project config holds the provider `one`, which answers
/// through `base` on the Anthropic wire.
async fn anthropic_config(dir: &Path, base: &str) -> PathBuf {
    let config = dir.join("config");
    tokio::fs::create_dir_all(config.join("reagent"))
        .await
        .expect("the config folder");
    let table = format!("wire = \"anthropic\"\nbase_url = \"{base}\"\napi_key = \"sk-test\"");
    let file = config.join("reagent").join("config.toml");
    tokio::fs::write(&file, format!("[providers.one]\n{table}\n"))
        .await
        .expect("the config");
    tokio::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .await
        .expect("the config is private");
    config
}

/// A steered run the test started: its stdout, its stdin and its exit status.
struct Run {
    child: Child,
    out: tokio::io::BufReader<tokio::process::ChildStdout>,
    stdin: Option<tokio::process::ChildStdin>,
    /// Every line read so far, newline included.
    seen: String,
}

/// Starts a run of `args` in `cwd`, with the config folder `config` and the
/// state folder `state`.
async fn start(state: &Path, cwd: &Path, config: &Path, args: &[&str]) -> Run {
    let mut child = Command::new(BIN)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_CACHE_HOME", "/nonexistent/reagent-test-cache")
        .env("XDG_STATE_HOME", state)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the run starts");
    Run {
        out: tokio::io::BufReader::new(child.stdout.take().expect("piped stdout")),
        stdin: child.stdin.take(),
        child,
        seen: String::new(),
    }
}

impl Run {
    /// Writes one request line on the run's stdin.
    async fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("piped stdin");
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("writes the request");
        stdin.flush().await.expect("flushes the request");
    }

    /// Closes the run's stdin, which reads as its end.
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// The next line of the run's stdout, without its newline.
    async fn line(&mut self) -> String {
        let mut line = String::new();
        tokio::time::timeout(WAIT, self.out.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("no line in time after: {:?}", self.seen))
            .expect("the run writes a line");
        assert!(
            !line.is_empty(),
            "the run closed its stdout early: {:?}",
            self.seen
        );
        self.seen.push_str(&line);
        line.trim_end().to_string()
    }

    /// The lines of the run's stdout up to the first one that holds `needle`,
    /// that one last.
    async fn until(&mut self, needle: &str) -> String {
        loop {
            let line = self.line().await;
            if line.contains(needle) {
                return line;
            }
        }
    }

    /// The rest of the stdout and the exit status of the run.
    async fn finish(mut self) -> (std::process::ExitStatus, String) {
        let mut rest = String::new();
        tokio::time::timeout(WAIT, self.out.read_to_string(&mut rest))
            .await
            .expect("the run ends in time")
            .expect("the run's stdout reads");
        self.seen.push_str(&rest);
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("the run exits in time")
            .expect("the run exits");
        (status, self.seen)
    }

    /// The rest of the stdout and the exit status, which must come within
    /// [`SOON`].
    async fn finish_soon(mut self) -> (std::process::ExitStatus, String) {
        let mut rest = String::new();
        tokio::time::timeout(SOON, self.out.read_to_string(&mut rest))
            .await
            .expect("the run ends soon")
            .expect("the run's stdout reads");
        self.seen.push_str(&rest);
        let status = tokio::time::timeout(SOON, self.child.wait())
            .await
            .expect("the run exits soon")
            .expect("the run exits");
        (status, self.seen)
    }

    /// The socket and the session the start line named.
    fn start_line(&self) -> Value {
        for line in self.seen.lines() {
            if let Ok(line) = serde_json::from_str::<Value>(line)
                && line["type"] == "start"
            {
                return line;
            }
        }
        panic!("no start line in {:?}", self.seen);
    }
}

/// One client of a listening run: its connection and the frames it reads.
struct Client {
    stream: UnixStream,
    buf: Vec<u8>,
    at: usize,
    next: u64,
}

impl Client {
    /// Joins the run of `run` with the token of its entry.
    async fn initialize(run: &Run) -> Client {
        let start = run.start_line();
        let socket = PathBuf::from(start["socket"].as_str().expect("a socket path"));
        let session = start["session"].as_str().expect("a session id");
        let entry = socket.with_file_name(format!("{session}.json"));
        let bytes = tokio::fs::read(&entry).await.expect("the entry file");
        let entry: Value = serde_json::from_slice(&bytes).expect("the entry is JSON");
        let token = entry["token"].as_str().expect("a token").to_string();
        let stream = tokio::time::timeout(WAIT, UnixStream::connect(&socket))
            .await
            .expect("a connection in time")
            .expect("the run accepts a client");
        let mut client = Client {
            stream,
            buf: Vec::new(),
            at: 0,
            next: 0,
        };
        let request = client
            .request(
                "initialize",
                &format!(
                    "{{\"major\":1,\"client\":{{\"name\":\"t\",\"version\":\"1\"}},\
                     \"capabilities\":[],\"token\":\"{token}\"}}"
                ),
            )
            .await;
        let answer = client.answer(request).await;
        let id = answer["result"]["client"].as_u64().expect("a client id");
        assert!(id > 0, "{answer}");
        let from = answer["result"]["committed"].as_u64().expect("a record");
        // A client sees the frames of the runs after the record it attaches
        // from.
        let request = client
            .request("attach", &format!("{{\"from\":{from}}}"))
            .await;
        client.answer(request).await;
        client
    }

    /// Sends one request and returns the id it carries.
    async fn request(&mut self, method: &str, params: &str) -> u64 {
        self.next += 1;
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"{method}\",\"params\":{params}}}\n",
            self.next
        );
        self.stream
            .write_all(line.as_bytes())
            .await
            .expect("writes the request");
        self.next
    }

    /// The next line without its newline, or `None` when the run closed the
    /// connection.
    async fn line(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(len) = self.buf[self.at..].iter().position(|byte| *byte == b'\n') {
                let start = self.at;
                self.at += len + 1;
                return Some(self.buf[start..start + len].to_vec());
            }
            if self.at > CHUNK {
                self.buf.drain(..self.at);
                self.at = 0;
            }
            let mut chunk = [0u8; CHUNK];
            let read = tokio::time::timeout(WAIT, self.stream.read(&mut chunk))
                .await
                .expect("a frame in time")
                .expect("the connection reads");
            if read == 0 {
                return None;
            }
            self.buf.extend_from_slice(&chunk[..read]);
        }
    }

    /// The answer of `request`, and the frames before it.
    async fn answer(&mut self, request: u64) -> Value {
        loop {
            match self.line().await {
                Some(line) => {
                    let frame: Value = serde_json::from_slice(&line).expect("a frame is JSON");
                    if frame["id"].as_u64() == Some(request) {
                        return frame;
                    }
                }
                None => panic!("the run closed the connection before the answer"),
            }
        }
    }

    /// The frames up to the first one that holds `needle` in its type.
    async fn until(&mut self, needle: &str) -> Vec<Value> {
        let mut taken = Vec::new();
        loop {
            match self.line().await {
                Some(line) => {
                    let frame: Value = serde_json::from_slice(&line).expect("a frame is JSON");
                    let found = frame["params"]["type"].as_str() == Some(needle)
                        || frame["type"].as_str() == Some(needle);
                    taken.push(frame);
                    if found {
                        return taken;
                    }
                }
                None => panic!("the run closed the connection; read {taken:?}"),
            }
        }
    }
}

#[tokio::test]
async fn a_steer_from_stdin_reaches_the_next_request() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![
        call_reply("bash", r#"{"command":"sleep 1"}"#),
        http_reply("done"),
    ])
    .await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    run.until("\"tool_start\"").await;
    run.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"stop after this","mode":"steer"}}"#,
    )
    .await;
    run.close_stdin();
    let answer = run.until("\"id\":1").await;
    assert!(answer.contains("\"result\""), "{answer}");
    assert!(answer.contains("\"disposition\":\"queued\""), "{answer}");
    let landed = run.until("\"user_message\"").await;
    assert!(landed.contains("stop after this"), "{landed}");
    assert!(landed.contains("\"mode\":\"steer\""), "{landed}");
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let first = server.body().await;
    assert!(first.contains("go"), "{first}");
    let next = server.body().await;
    assert!(next.contains("stop after this"), "{next}");
}

#[tokio::test]
async fn a_steer_lands_with_its_message_id() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![
        call_reply("bash", r#"{"command":"sleep 1"}"#),
        http_reply("done"),
    ])
    .await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    run.until("\"tool_start\"").await;
    run.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"stop after this","mode":"steer"}}"#,
    )
    .await;
    run.close_stdin();
    let answer = run.until("\"id\":1").await;
    let answer: Value = serde_json::from_str(&answer).expect("the answer is JSON");
    let message = answer["result"]["message_id"]
        .as_u64()
        .expect("the answer names the message");
    assert!(message >= 1, "{answer}");
    let accepted = run.until("\"user_message\"").await;
    let accepted: Value = serde_json::from_str(&accepted).expect("the line is JSON");
    assert_eq!(accepted["message_id"].as_u64(), Some(message), "{accepted}");
    let landed = run.until("\"message_state\"").await;
    let landed: Value = serde_json::from_str(&landed).expect("the line is JSON");
    assert_eq!(landed["message_id"].as_u64(), Some(message), "{landed}");
    assert_eq!(landed["state"], "landed", "{landed}");
    // The next request follows the state, so the message landed before it.
    run.until("\"turn_start\"").await;
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let first = server.body().await;
    assert!(first.contains("go"), "{first}");
    let next = server.body().await;
    assert!(next.contains("stop after this"), "{next}");
}

#[tokio::test]
async fn a_cancel_drops_a_queued_message() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), &slow_script("sleep 30")).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--provider", &fake, "-p", "go"],
    )
    .await;
    run.until("\"tool_start\"").await;
    run.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"later","mode":"queue"}}"#,
    )
    .await;
    let answer = run.until("\"id\":1").await;
    let answer: Value = serde_json::from_str(&answer).expect("the answer is JSON");
    assert_eq!(answer["result"]["disposition"], "queued", "{answer}");
    let message = answer["result"]["message_id"]
        .as_u64()
        .expect("the answer names the message");
    run.send(r#"{"jsonrpc":"2.0","id":2,"method":"cancel"}"#)
        .await;
    run.until("\"id\":2").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(1), "{out}");
    let dropped = out
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| {
            line["type"] == "message_state" && line["message_id"].as_u64() == Some(message)
        })
        .unwrap_or_else(|| panic!("a message_state line for {message} in {out}"));
    assert_eq!(dropped["message_id"].as_u64(), Some(message), "{dropped}");
    assert_eq!(dropped["state"], "dropped", "{dropped}");
    assert_eq!(dropped["reason"], "cancelled", "{dropped}");
}

#[tokio::test]
async fn two_equal_texts_get_two_ids() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(
        tmp.path(),
        r#"{"replies":[{"text":"one"},{"text":"two"},{"text":"three"}]}"#,
    )
    .await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--provider", &fake, "-p", "go"],
    )
    .await;
    run.until("\"turn_start\"").await;
    let mut ids = Vec::new();
    for id in [1, 2] {
        run.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"send","params":{{"text":"same","mode":"queue"}}}}"#
        ))
        .await;
        let answer = run.until(&format!("\"id\":{id}")).await;
        let answer: Value = serde_json::from_str(&answer).expect("the answer is JSON");
        ids.push(
            answer["result"]["message_id"]
                .as_u64()
                .expect("the answer names the message"),
        );
    }
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert_ne!(ids[0], ids[1], "two sends get two ids: {out}");
    let lines: Vec<Value> = out
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    for message in ids {
        let states: Vec<&Value> = lines
            .iter()
            .filter(|line| {
                line["type"] == "message_state" && line["message_id"].as_u64() == Some(message)
            })
            .collect();
        assert_eq!(states.len(), 1, "one state for {message}: {out}");
        assert_eq!(states[0]["state"], "landed", "{out}");
    }
}

#[tokio::test]
async fn a_cancel_from_stdin_ends_the_run() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), &slow_script("sleep 30")).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--provider", &fake, "-p", "go"],
    )
    .await;
    run.until("\"tool_start\"").await;
    run.send(r#"{"jsonrpc":"2.0","id":2,"method":"cancel"}"#)
        .await;
    let answer = run.until("\"id\":2").await;
    assert!(answer.contains("\"result\""), "{answer}");
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(1), "{out}");
    assert!(out.contains("\"run_end\""), "{out}");
    assert!(out.contains("\"reason\":\"cancelled\""), "{out}");
}

#[tokio::test]
async fn a_bad_line_on_stdin_is_answered_and_the_run_goes_on() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--provider", &fake, "-p", "go"],
    )
    .await;
    run.send("not json").await;
    run.send(
        r#"{"jsonrpc":"2.0","id":9,"method":"initialize","params":{"major":1,"client":{"name":"t","version":"1"},"capabilities":[],"token":"x"}}"#,
    )
    .await;
    run.until("\"id\":9").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(out.contains("parse error"), "{out}");
    let errors = out.matches("\"error\"").count();
    assert_eq!(errors, 2, "{out}");
}

#[tokio::test]
async fn stdin_answers_an_approval() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    ask_folder(tmp.path()).await;
    let fake = script(tmp.path(), &asked_script("echo ran >> once.txt")).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--provider", &fake, "-p", "go"],
    )
    .await;
    let asked = run.until("\"approval_request\"").await;
    let ask: Value = serde_json::from_str(&asked).expect("the ask is JSON");
    let ask = ask["id"].as_u64().expect("the ask names its id");
    run.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"approve","params":{{"id":{ask},"answer":"once"}}}}"#
    ))
    .await;
    run.until("\"approval_resolved\"").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(out.contains("\"tool_result\""), "{out}");
    assert!(!out.contains("\"is_error\":true"), "{out}");
    let ran = tokio::fs::read_to_string(tmp.path().join("once.txt"))
        .await
        .expect("the call ran");
    assert_eq!(ran, "ran\n", "the call ran once");
}

#[tokio::test]
async fn no_task_the_first_send_starts_the_run() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), r#"{"replies":[{"text":"hello there"}]}"#).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--provider", &fake],
    )
    .await;
    run.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"hello","mode":"queue"}}"#,
    )
    .await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(out.contains("hello there"), "{out}");
    assert!(out.contains("\"disposition\":\"started\""), "{out}");
    assert!(
        out.lines()
            .any(|line| line.contains("\"user_message\"") && line.contains("hello")),
        "{out}"
    );
}

#[tokio::test]
async fn closing_stdin_does_not_cancel_the_run() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), &slow_script("sleep 1")).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--provider", &fake, "-p", "go"],
    )
    .await;
    run.until("\"tool_start\"").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(out.contains("\"reason\":\"end_turn\""), "{out}");
    assert!(out.contains("done"), "{out}");
}

#[tokio::test]
async fn a_client_steers_a_listening_run() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![
        call_reply("bash", r#"{"command":"sleep 1"}"#),
        http_reply("done"),
    ])
    .await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--listen", "--model", "one/m", "-p", "go"],
    )
    .await;
    run.until("\"type\":\"start\"").await;
    let start = run.start_line();
    assert!(
        start["socket"].is_string(),
        "the start line names no socket"
    );
    assert!(
        start["session"].is_string(),
        "the start line names no session"
    );
    let printed = run.seen.clone();
    assert!(!printed.contains("token"), "{printed}");
    let mut client = Client::initialize(&run).await;
    client
        .request("send", r#"{"text":"stop after this","mode":"steer"}"#)
        .await;
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let first = server.body().await;
    assert!(first.contains("go"), "{first}");
    let next = server.body().await;
    assert!(next.contains("stop after this"), "{next}");
}

#[tokio::test]
async fn an_attached_client_does_not_hold_the_run() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), &slow_script("sleep 1")).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--listen", "--provider", &fake, "-p", "go"],
    )
    .await;
    run.until("\"type\":\"start\"").await;
    let mut client = Client::initialize(&run).await;
    // The client is still attached when the run ends, and it gets that frame
    // before the connection goes.
    client.until("run_end").await;
    let (status, out) = run.finish_soon().await;
    assert_eq!(status.code(), Some(0), "{out}");
}

#[tokio::test]
async fn an_approval_waits_until_the_deadline() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    ask_folder(tmp.path()).await;
    let fake = script(tmp.path(), &asked_script("echo ran >> once.txt")).await;
    let run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &[
            "--json",
            "--listen",
            "--deadline",
            "3",
            "--provider",
            &fake,
            "-p",
            "go",
        ],
    )
    .await;
    let (status, out) = run.finish_soon().await;
    assert_eq!(status.code(), Some(124), "{out}");
    assert!(out.contains("\"exit\":\"cancelled\""), "{out}");
}

#[tokio::test]
async fn a_no_serve_run_opens_no_socket() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--no-serve", "--provider", &fake, "-p", "go"],
    )
    .await;
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(!out.contains("\"socket\""), "{out}");
    assert!(!out.contains("\"user_message\""), "{out}");
    assert!(!out.contains("\"message_state\""), "{out}");
    let start: Value = out
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("a start line");
    let session = start["session"].as_str().expect("a session id");
    let folder = state.path().join("discovery");
    if let Ok(mut entries) = tokio::fs::read_dir(&folder).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            assert_ne!(
                entry.file_name().to_string_lossy(),
                format!("{session}.json"),
                "a plain run wrote an entry"
            );
        }
    }
}

#[tokio::test]
async fn commands_without_json_is_refused() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = Command::new(BIN)
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .args(["--commands", "-p", "go"])
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--json"), "{stderr}");
}

/// A resumed session whose open call asks for an approval takes the answer
/// from stdin: the stdin client reads before the open calls run.
#[tokio::test]
async fn stdin_answers_an_approval_of_a_resumed_call() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let config = Path::new("/nonexistent/reagent-test-config");
    let started = tmp.path().join("started.txt");
    tokio::fs::write(tmp.path().join("data.txt"), "hello\n")
        .await
        .expect("the file the read takes");
    // The session is found by its project folder, so the repo is there from
    // the first run on.
    tokio::fs::create_dir(tmp.path().join(".git"))
        .await
        .expect("the repo folder");
    // The bash call is killed in its sleep and reported as cut; the read is
    // never reported so, and the continue runs it again, by then under a rule
    // that asks a person for it.
    let first = serde_json::json!({
        "replies": [{"calls": [
            {"name": "bash", "arguments": {"command": format!("echo go >> {}; sleep 30", started.display())}},
            {"name": "read", "arguments": {"path": "data.txt"}}
        ]}]
    })
    .to_string();
    let fake = script(tmp.path(), &first).await;
    let mut killed = start(
        state.path(),
        tmp.path(),
        config,
        &["--json", "--provider", &fake, "-p", "go"],
    )
    .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while tokio::fs::metadata(&started).await.is_err() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first call never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    killed.child.kill().await.expect("the first run is killed");
    drop(killed);
    tokio::fs::create_dir_all(tmp.path().join(".reagent"))
        .await
        .expect("the config folder");
    tokio::fs::write(
        tmp.path().join(".reagent").join("config.toml"),
        "[permissions]\nask = [\"Read\"]\n",
    )
    .await
    .expect("the rules");

    let fake = script(tmp.path(), r#"{"replies":[{"text":"done"}]}"#).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        config,
        &["-c", "--json", "--commands", "--provider", &fake],
    )
    .await;
    let asked = run.until("\"approval_request\"").await;
    let ask: Value = serde_json::from_str(&asked).expect("the ask is JSON");
    let ask = ask["id"].as_u64().expect("the ask names its id");
    run.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"approve","params":{{"id":{ask},"answer":"once"}}}}"#
    ))
    .await;
    run.until("\"approval_resolved\"").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let read = out
        .lines()
        .find(|line| line.contains("\"tool_result\"") && line.contains("call_1"))
        .unwrap_or_else(|| panic!("the open read ran: {out}"));
    assert!(!read.contains("\"is_error\":true"), "{read}");
}

/// Runs a session whose first request a server takes and never answers, then
/// kills the run, so the turn of its message is left open.
async fn kill_in_the_first_request(state: &Path, tmp: &Path) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    let config = anthropic_config(tmp, &base).await;
    let mut killed = start(
        state,
        tmp,
        &config,
        &["--json", "--model", "one/m", "-p", "hello"],
    )
    .await;
    let (_socket, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the run sends its request")
        .expect("the request arrives");
    // The message is on the disk before its request goes out; give the writer a
    // moment to finish the write.
    tokio::time::sleep(Duration::from_millis(200)).await;
    killed.child.kill().await.expect("the run is killed");
}

/// A steered continue with no task asks for the reply a killed run left owed
/// before it waits for a client.
#[tokio::test]
async fn continue_answers_an_open_turn() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    kill_in_the_first_request(state.path(), tmp.path()).await;
    let mut server = serve(vec![http_reply("the reply")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["-c", "--json", "--commands"],
    )
    .await;
    // The reply comes while stdin is still open and nothing went on it.
    run.until("the reply").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let body = server.body().await;
    assert!(body.contains("hello"), "{body}");
    assert_eq!(server.bodies.len(), 0, "one request only: {out}");
}

/// The reply a steered continue asks for ends its turn, and the session then
/// waits for a client: nothing asks the model again on its own.
#[tokio::test]
async fn continue_waits_when_the_turn_ended() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    kill_in_the_first_request(state.path(), tmp.path()).await;
    let mut server = serve(vec![http_reply("the reply"), http_reply("next reply")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["-c", "--json", "--commands"],
    )
    .await;
    run.until("the reply").await;
    let owed = server.body().await;
    assert!(owed.contains("hello"), "{owed}");
    assert!(
        tokio::time::timeout(Duration::from_secs(1), server.bodies.recv())
            .await
            .is_err(),
        "the session asked again with no request from a client"
    );
    run.send(r#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"next","mode":"queue"}}"#)
        .await;
    let answer = run.until("\"id\":1").await;
    assert!(answer.contains("\"result\""), "{answer}");
    let landed = run.until("next reply").await;
    assert!(landed.contains("next reply"), "{landed}");
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let body = server.body().await;
    assert!(body.contains("next"), "{body}");
    assert_eq!(server.bodies.len(), 0, "two requests only: {out}");
}

/// The reply of a steered continue stops on a cancel from stdin, as any run
/// of the session does.
#[tokio::test]
async fn continue_takes_a_cancel() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    kill_in_the_first_request(state.path(), tmp.path()).await;
    let fake = script(tmp.path(), &slow_script("sleep 30")).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["-c", "--json", "--commands", "--provider", &fake],
    )
    .await;
    run.until("\"tool_start\"").await;
    run.send(r#"{"jsonrpc":"2.0","id":1,"method":"cancel"}"#)
        .await;
    let answer = run.until("\"id\":1").await;
    assert!(answer.contains("\"result\""), "{answer}");
    run.close_stdin();
    let (status, out) = run.finish_soon().await;
    assert_eq!(status.code(), Some(1), "{out}");
    assert!(out.contains("\"reason\":\"cancelled\""), "{out}");
}

/// A steered continue of a session whose last turn ended asks nothing on its
/// own: it opens the session and waits for a client's message.
#[tokio::test]
async fn continue_opens_a_finished_session_and_waits() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![http_reply("first reply"), http_reply("next reply")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let first = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--model", "one/m", "-p", "hello"],
    )
    .await;
    let (status, out) = first.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    server.body().await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["-c", "--json", "--commands"],
    )
    .await;
    run.until("\"start\"").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), server.bodies.recv())
            .await
            .is_err(),
        "a finished session asked the model with no message"
    );
    run.send(r#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"next","mode":"queue"}}"#)
        .await;
    run.until("next reply").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let body = server.body().await;
    assert!(body.contains("next"), "{body}");
    assert_eq!(
        server.bodies.len(),
        0,
        "one request after the continue: {out}"
    );
}

/// A listening run with no task waits for a client, and the client finds the
/// socket in the first line, so that line comes before any event.
#[tokio::test]
async fn a_listening_run_with_no_task_names_its_socket_at_once() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(tmp.path(), r#"{"replies":[{"text":"unused"}]}"#).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--commands", "--listen", "--provider", &fake],
    )
    .await;
    let first = run.line().await;
    assert!(first.contains("\"type\":\"start\""), "{first}");
    assert!(first.contains("\"socket\""), "{first}");
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
}

/// The questions one `ask` call carries: which database, with two labels.
const ASK_CALL: &str = r#"{"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres"}]}]}"#;

/// The names of the tools a recorded request body carries.
fn tool_names(body: &str) -> Vec<String> {
    let body: Value = serde_json::from_str(body).expect("a JSON body");
    body["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn ask_stdin_answers_a_question() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![call_reply("ask", ASK_CALL), http_reply("done")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    let asked = run.until("\"question_request\"").await;
    let ask: Value = serde_json::from_str(&asked).expect("the ask is JSON");
    assert_eq!(ask["questions"][0]["q"], "Which database?", "{asked}");
    assert_eq!(ask["questions"][0]["options"][1]["label"], "postgres");
    let id = ask["id"].as_u64().expect("the ask names its id");
    run.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"answer","params":{{"id":{id},"answers":[["sqlite"]]}}}}"#
    ))
    .await;
    run.until("\"question_resolved\"").await;
    run.until("done").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    server.body().await;
    let next = server.body().await;
    assert!(next.contains("A: sqlite"), "{next}");
}

#[tokio::test]
async fn ask_an_own_answer_reaches_the_model() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![call_reply("ask", ASK_CALL), http_reply("done")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    let asked = run.until("\"question_request\"").await;
    let ask: Value = serde_json::from_str(&asked).expect("the ask is JSON");
    let id = ask["id"].as_u64().expect("the ask names its id");
    run.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"answer","params":{{"id":{id},"answers":[["duckdb"]]}}}}"#
    ))
    .await;
    run.until("\"question_resolved\"").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    server.body().await;
    let next = server.body().await;
    assert!(next.contains("A: duckdb"), "{next}");
}

#[tokio::test]
async fn ask_two_questions_get_two_answers() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let two = r#"{"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres"}]},{"q":"Which cache?","options":[{"label":"redis"},{"label":"none"}]}]}"#;
    let mut server = serve(vec![call_reply("ask", two), http_reply("done")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    let asked = run.until("\"question_request\"").await;
    let ask: Value = serde_json::from_str(&asked).expect("the ask is JSON");
    assert_eq!(ask["questions"].as_array().expect("questions").len(), 2);
    let id = ask["id"].as_u64().expect("the ask names its id");
    run.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"answer","params":{{"id":{id},"answers":[["sqlite"],["redis"]]}}}}"#
    ))
    .await;
    run.until("\"question_resolved\"").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    server.body().await;
    let next = server.body().await;
    assert!(next.contains("A: sqlite"), "{next}");
    assert!(next.contains("A: redis"), "{next}");
}

#[tokio::test]
async fn ask_a_skip_gives_the_fallback() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![call_reply("ask", ASK_CALL), http_reply("done")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    let asked = run.until("\"question_request\"").await;
    let ask: Value = serde_json::from_str(&asked).expect("the ask is JSON");
    let id = ask["id"].as_u64().expect("the ask names its id");
    run.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"answer","params":{{"id":{id},"answers":null}}}}"#
    ))
    .await;
    run.until("\"question_resolved\"").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    server.body().await;
    let next = server.body().await;
    assert!(
        next.contains("decide yourself and state the choice"),
        "{next}"
    );
}

#[tokio::test]
async fn ask_a_cancel_resolves_the_question() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![call_reply("ask", ASK_CALL)]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    run.until("\"question_request\"").await;
    run.send(r#"{"jsonrpc":"2.0","id":1,"method":"cancel"}"#)
        .await;
    let answer = run.until("\"id\":1").await;
    assert!(answer.contains("\"result\""), "{answer}");
    run.close_stdin();
    let (status, out) = run.finish_soon().await;
    assert_eq!(status.code(), Some(1), "{out}");
    assert!(out.contains("\"reason\":\"cancelled\""), "{out}");
    server.bodies.close();
}

#[tokio::test]
async fn ask_a_plain_run_offers_no_ask() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![http_reply("ok")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--model", "one/m", "-p", "go"],
    )
    .await;
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let body = server.body().await;
    assert!(
        !tool_names(&body).iter().any(|name| name == "ask"),
        "{body}"
    );
    assert!(
        tool_names(&body).iter().any(|name| name == "bash"),
        "{body}"
    );
}

#[tokio::test]
async fn ask_a_steered_run_offers_ask() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![http_reply("ok")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &["--json", "--commands", "--model", "one/m", "-p", "go"],
    )
    .await;
    run.until("ok").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let body = server.body().await;
    assert!(tool_names(&body).iter().any(|name| name == "ask"), "{body}");
}

#[tokio::test]
async fn ask_tools_minus_ask_removes_it() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let mut server = serve(vec![http_reply("ok")]).await;
    let config = anthropic_config(tmp.path(), &server.base).await;
    let mut run = start(
        state.path(),
        tmp.path(),
        &config,
        &[
            "--json",
            "--commands",
            "--tools",
            "-ask",
            "--model",
            "one/m",
            "-p",
            "go",
        ],
    )
    .await;
    run.until("ok").await;
    run.close_stdin();
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    let body = server.body().await;
    assert!(
        !tool_names(&body).iter().any(|name| name == "ask"),
        "{body}"
    );
}
