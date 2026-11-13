//! End-to-end tests of a served `-p` run: a run with a session that serves its
//! socket by default, so a client can steer, queue or cancel it, while it prints
//! and ends as a plain run does.
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

/// A state folder deep enough that the socket path inside it is over the
/// platform's cap: two folders of forty characters, under the short `/tmp` one.
async fn deep_state() -> (tempfile::TempDir, PathBuf) {
    let dir = state();
    let deep = dir.path().join("d".repeat(40)).join("e".repeat(40));
    tokio::fs::create_dir_all(&deep)
        .await
        .expect("the deep folder");
    (dir, deep)
}

/// A scripted provider reply, and the `--provider` value that reads it.
async fn script(dir: &Path, body: &str) -> String {
    let path = dir.join("script.json");
    tokio::fs::write(&path, body)
        .await
        .expect("writes the script");
    format!("fake:{}", path.display())
}

/// A reply that calls `bash` with `command`, and a reply that ends the run.
fn bash_script(command: &str) -> String {
    serde_json::json!({
        "replies": [
            {"calls": [{"name": "bash", "arguments": {"command": command}}]},
            {"text": "done"}
        ]
    })
    .to_string()
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

/// A run the test started: its stdout, its stdin and its exit status.
struct Run {
    child: Child,
    out: tokio::io::BufReader<tokio::process::ChildStdout>,
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
        child,
        seen: String::new(),
    }
}

impl Run {
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

    /// The start line the run printed.
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

/// One client of a serving run: its connection and the frames it reads.
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
}

/// The output of a run `args` over `state`, with its stderr.
async fn output(state: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(BIN);
    command
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_CACHE_HOME", "/nonexistent/reagent-test-cache")
        .env("XDG_STATE_HOME", state)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null());
    tokio::time::timeout(WAIT, command.output())
        .await
        .expect("the run ends in time")
        .expect("the binary runs")
}

/// The `--json` lines of `out` after its start line, with the `start` line
/// dropped.
fn lines_after_start(out: &str) -> Vec<Value> {
    out.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["type"] != "start")
        .collect()
}

/// A served `-p` run names its session and its socket in the first line, and a
/// client finds it there and queues the message it takes next.
#[tokio::test]
async fn a_served_run_names_its_socket_and_takes_a_queued_message() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let fake = script(
        tmp.path(),
        &serde_json::json!({
            "replies": [
                {"calls": [{"name": "bash", "arguments": {"command": "sleep 2"}}]},
                {"text": "done"},
                {"text": "another answer"}
            ]
        })
        .to_string(),
    )
    .await;
    let mut run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &["--json", "--provider", &fake, "-p", "go"],
    )
    .await;
    // The run serves from its first line, so a client can join before any event.
    let first = run.line().await;
    let start: Value = serde_json::from_str(&first).expect("the start line is JSON");
    assert_eq!(start["type"], "start", "{first}");
    assert!(start["session"].is_string(), "no session: {first}");
    assert!(start["socket"].is_string(), "no socket: {first}");
    assert!(!first.contains("token"), "{first}");
    let mut client = Client::initialize(&run).await;
    let request = client
        .request("send", r#"{"text":"and then this","mode":"queue"}"#)
        .await;
    let answer = client.answer(request).await;
    assert_eq!(answer["result"]["disposition"], "queued", "{answer}");
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(
        out.contains("another answer"),
        "the queued message ran: {out}"
    );
    assert_eq!(
        out.matches("\"type\":\"run_end\"").count(),
        2,
        "the run took the queued message as a second run: {out}"
    );
}

/// `--no-serve` turns the socket off, and the run leaves no entry behind.
#[tokio::test]
async fn a_no_serve_run_has_no_socket() {
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
    let start: Value = out
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("a start line");
    assert!(start["session"].is_string(), "{start}");
    let session = start["session"].as_str().expect("a session id");
    let folder = state.path().join("reagent/workers/v1");
    if let Ok(mut entries) = tokio::fs::read_dir(&folder).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            assert_ne!(
                entry.file_name().to_string_lossy(),
                format!("{session}.json"),
                "a no-serve run wrote an entry"
            );
        }
    }
}

/// A served run prints the lines of a plain run: taking clients changes nothing
/// it writes but the socket of its start line.
#[tokio::test]
async fn a_served_run_writes_the_lines_of_a_plain_run() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), &bash_script("echo hello")).await;
    let served = {
        let state = state();
        let run = start(
            state.path(),
            tmp.path(),
            Path::new("/nonexistent/reagent-test-config"),
            &["--json", "--provider", &fake, "-p", "go"],
        )
        .await;
        let (status, out) = run.finish().await;
        assert_eq!(status.code(), Some(0), "{out}");
        out
    };
    let plain = {
        let state = state();
        let run = start(
            state.path(),
            tmp.path(),
            Path::new("/nonexistent/reagent-test-config"),
            &["--json", "--no-serve", "--provider", &fake, "-p", "go"],
        )
        .await;
        let (status, out) = run.finish().await;
        assert_eq!(status.code(), Some(0), "{out}");
        out
    };
    assert!(
        served
            .lines()
            .next()
            .is_some_and(|line| line.contains("\"socket\"")),
        "the first run served its socket: {served}"
    );
    // The start line names the socket of one run only, and it is dropped. No
    // other line carries a timing or an id that two runs cannot share: the tool
    // call id is the script's own.
    let served = lines_after_start(&served);
    let plain = lines_after_start(&plain);
    assert!(!served.is_empty(), "no lines without the start line");
    assert_eq!(served, plain, "the served run prints other lines");
}

/// A served run offers the tool list of a plain run: no `ask`.
#[tokio::test]
async fn a_served_run_offers_no_ask() {
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
        "a served run offered ask: {body}"
    );
    assert!(
        tool_names(&body).iter().any(|name| name == "bash"),
        "{body}"
    );
}

/// A call the rules send to a person is refused at once in a served run: no
/// client answers it, and the run does not wait for one.
#[tokio::test]
async fn a_served_run_refuses_an_approval_at_once() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    ask_folder(tmp.path()).await;
    let fake = script(tmp.path(), &bash_script("echo ran >> once.txt")).await;
    let run = start(
        state.path(),
        tmp.path(),
        Path::new("/nonexistent/reagent-test-config"),
        &[
            "--json",
            "--deadline",
            "20",
            "--provider",
            &fake,
            "-p",
            "go",
        ],
    )
    .await;
    let began = tokio::time::Instant::now();
    let (status, out) = run.finish_soon().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "the run waited for a client: {:?}",
        began.elapsed()
    );
    assert!(
        !out.contains("\"approval_request\""),
        "the refusal went to a client: {out}"
    );
    let refused = out
        .lines()
        .find(|line| line.contains("\"tool_result\""))
        .unwrap_or_else(|| panic!("the refused call has a result: {out}"));
    assert!(refused.contains("\"is_error\":true"), "{refused}");
    assert!(out.contains("done"), "the run went on: {out}");
    assert!(
        tokio::fs::metadata(tmp.path().join("once.txt"))
            .await
            .is_err(),
        "the refused call ran"
    );
}

/// A socket path over the platform's cap is a warning and a plain run, and the
/// same folder with `--listen` fails.
#[tokio::test]
async fn a_long_socket_path_warns_and_runs() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let (_dir, deep) = deep_state().await;
    let fake = script(tmp.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let out = output(
        &deep,
        tmp.path(),
        &["--json", "--provider", &fake, "-p", "go"],
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let warnings = stderr
        .lines()
        .filter(|line| line.starts_with("reagent: warning: "))
        .count();
    assert_eq!(warnings, 1, "one warning line in {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"type\":\"start\""), "{stdout}");
    assert!(!stdout.contains("\"socket\""), "{stdout}");
    let listened = output(
        &deep,
        tmp.path(),
        &["--json", "--listen", "--provider", &fake, "-p", "go"],
    )
    .await;
    assert_ne!(listened.status.code(), Some(0), "{listened:?}");
}

/// A served run that cannot prepare an `@image` file stops before any request
/// and exits 1, as a plain run does.
#[tokio::test]
async fn a_served_run_stops_on_a_broken_image() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    // A PNG header and nothing decodable after it.
    let mut broken = b"\x89PNG\r\n\x1a\n".to_vec();
    broken.extend_from_slice(&[0_u8; 24]);
    tokio::fs::write(tmp.path().join("bad.png"), &broken)
        .await
        .expect("the broken image");
    let fake = script(tmp.path(), r#"{"replies":[{"text":"a reply"}]}"#).await;
    let out = output(
        state.path(),
        tmp.path(),
        &["--json", "--provider", &fake, "-p", "look at @bad.png"],
    )
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("bad.png"), "{stderr}");
    assert!(
        !stderr.contains("reagent: warning: "),
        "the run did not serve: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("a reply"), "a request went out: {stdout}");
}

/// A plugin sees the end of a turn before a call of the next turn runs, also
/// when the printer is behind: the first reply is larger than the stdout pipe
/// and the test reads nothing for two seconds.
#[tokio::test]
async fn a_listener_sees_the_turn_end_before_the_next_call() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let state = state();
    let config = tmp.path().join("config");
    let plugin = config.join("reagent").join("plugins").join("counting");
    tokio::fs::create_dir_all(&plugin)
        .await
        .expect("the plugin folder");
    tokio::fs::write(
        plugin.join("package.json"),
        r#"{"name": "counting", "reagent": {"entry": "index.js"}}"#,
    )
    .await
    .expect("the manifest");
    // `count` answers one byte more than the turns that ended, so the
    // `bytes` of its result line carries the count.
    tokio::fs::write(
        plugin.join("index.js"),
        r#"
let turns = 0;
export default function (reagent) {
  reagent.on("turn_end", () => { turns += 1; });
  reagent.registerTool({ name: "nothing", description: "Does nothing.", parameters: { type: "object" }, run: () => "" });
  reagent.registerTool({ name: "count", description: "Counts.", parameters: { type: "object" }, run: () => "x".repeat(turns + 1) });
}
"#,
    )
    .await
    .expect("the entry");
    let work = tmp.path().join("work");
    tokio::fs::create_dir_all(&work)
        .await
        .expect("the work folder");
    let big = "x".repeat(256 * 1024);
    let fake = script(
        tmp.path(),
        &serde_json::json!({"replies": [
          {"text": big, "calls": [{"id": "n1", "name": "nothing", "arguments": {}}]},
          {"text": "two", "calls": [{"id": "c1", "name": "count", "arguments": {}}]},
          {"text": "done"}
        ]})
        .to_string(),
    )
    .await;
    let run = start(
        state.path(),
        &work,
        &config,
        &["--json", "--provider", &fake, "-p", "count the turns"],
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (status, out) = run.finish().await;
    assert_eq!(status.code(), Some(0), "{out}");
    assert!(
        out.lines()
            .next()
            .is_some_and(|line| line.contains("\"socket\"")),
        "the run served its socket: {out}"
    );
    let counted = out
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["type"] == "tool_result")
        .expect("a tool result line");
    assert_eq!(counted["bytes"], 2, "{out}");
}
