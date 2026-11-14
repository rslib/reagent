//! End-to-end tests of `reagent rpc`: the helper a supervisor runs to read the
//! requests of a live session that wait for a person, and to answer them from
//! the lines of its stdin.
//!
//! Each test starts the binary against a scripted fake provider and a state
//! folder of its own, so these tests exist only with the `testing` feature.
#![cfg(feature = "testing")]

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use reagent_protocol::discovery::Entry;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for a line before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// How many bytes one read of a test client asks for.
const CHUNK: usize = 8192;

/// The script of a run whose first reply calls `bash` and whose next one ends
/// the run.
const APPROVAL: &str = r#"{"replies":[{"calls":[{"name":"bash","arguments":{"command":"echo hi"}}]},{"text":"done"}]}"#;

/// A state folder short enough for a socket path, which the platform caps.
fn state() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rw")
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

/// A worker the test started, with the ready line it printed.
struct Worker {
    child: tokio::process::Child,
    ready: Value,
}

impl Worker {
    /// Starts a worker and reads its ready line.
    async fn start(state: &Path, cwd: &Path, args: &[&str]) -> Worker {
        let mut child = Command::new(BIN)
            .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
            .env("XDG_CACHE_HOME", "/nonexistent/reagent-test-cache")
            .env("XDG_STATE_HOME", state)
            .arg("worker")
            .args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the worker starts");
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut line = String::new();
        tokio::time::timeout(
            WAIT,
            tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut line),
        )
        .await
        .expect("a ready line in time")
        .expect("the worker writes a line");
        let ready: Value = serde_json::from_str(line.trim_end()).unwrap_or_else(|error| {
            panic!("the ready line is JSON: {error}: {line:?}");
        });
        Worker { child, ready }
    }

    fn socket(&self) -> PathBuf {
        PathBuf::from(self.ready["socket"].as_str().expect("a socket path"))
    }

    fn entry_path(&self) -> PathBuf {
        PathBuf::from(self.ready["socket"].as_str().expect("a socket path"))
            .with_file_name(format!("{}.json", self.session()))
    }

    fn session(&self) -> &str {
        self.ready["session"].as_str().expect("a session id")
    }

    async fn entry(&self) -> Entry {
        let bytes = tokio::fs::read(self.entry_path())
            .await
            .expect("the entry file is there");
        serde_json::from_slice(&bytes).expect("the entry is JSON")
    }

    /// The entry, once it says `waiting`: the worker rewrites it after the
    /// frame of the change went out, so it lands a moment later.
    async fn marked(&self, waiting: bool) -> Entry {
        for _ in 0..100 {
            let entry = self.entry().await;
            if entry.waiting == waiting {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "the entry never said waiting = {waiting}: {:?}",
            self.entry().await
        );
    }

    async fn connect(&self) -> UnixStream {
        tokio::time::timeout(WAIT, UnixStream::connect(self.socket()))
            .await
            .expect("a connection in time")
            .expect("the worker accepts a client")
    }

    /// Stops the worker without waiting for its own exit.
    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// One client of a worker: its connection, the frames it reads and the ids the
/// worker gave it.
struct Client {
    stream: UnixStream,
    /// The bytes read, of which the first `at` are used.
    buf: Vec<u8>,
    /// How many bytes of `buf` the frames read so far used.
    at: usize,
    /// The frames read but not used yet, oldest first.
    frames: VecDeque<Value>,
    /// The id `initialize` gave this connection.
    id: u64,
    /// The newest record the worker had committed at `initialize`.
    committed: u64,
    /// The id of the next request this client sends.
    next: u64,
}

impl Client {
    /// Connects and initializes with `name`.
    async fn initialize(worker: &Worker, token: &str, name: &str) -> Client {
        let mut client = Client {
            stream: worker.connect().await,
            buf: Vec::new(),
            at: 0,
            frames: VecDeque::new(),
            id: 0,
            committed: 0,
            next: 0,
        };
        let request = client
            .request(
                "initialize",
                &format!(
                    "{{\"major\":1,\"client\":{{\"name\":\"{name}\",\"version\":\"1\"}},\
                     \"capabilities\":[],\"token\":\"{token}\"}}"
                ),
            )
            .await;
        let answer = client.answer(request).await;
        client.id = answer["result"]["client"].as_u64().expect("a client id");
        client.committed = answer["result"]["committed"].as_u64().expect("a record");
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

    /// The next line without its newline, or `None` when the worker closed the
    /// connection.
    async fn line(&mut self) -> Option<&[u8]> {
        loop {
            if let Some(len) = self.buf[self.at..].iter().position(|byte| *byte == b'\n') {
                let start = self.at;
                self.at += len + 1;
                return Some(&self.buf[start..start + len]);
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

    /// The next frame, or `None` when the worker closed the connection.
    async fn frame(&mut self) -> Option<Value> {
        let line = self.line().await?;
        Some(serde_json::from_slice(line).expect("a frame is JSON"))
    }

    /// The frames up to the first one `stop` holds for, that one last.
    async fn until(&mut self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut taken = Vec::new();
        loop {
            if let Some(at) = self.frames.iter().position(&stop) {
                taken.extend(self.frames.drain(..=at));
                return taken;
            }
            match self.frame().await {
                Some(frame) => self.frames.push_back(frame),
                None => panic!("the worker closed the connection; read {taken:?}"),
            }
        }
    }

    /// The answer of `request`, which keeps the frames read before it.
    async fn answer(&mut self, request: u64) -> Value {
        let mut frames = VecDeque::from(
            self.until(|frame| frame["id"].as_u64() == Some(request))
                .await,
        );
        let answer = frames.pop_back().expect("an answer");
        while let Some(frame) = frames.pop_back() {
            self.frames.push_front(frame);
        }
        answer
    }

    /// Attaches from `from`, and returns the frames the answer came after.
    async fn attach(&mut self, from: u64) -> Vec<Value> {
        let request = self
            .request("attach", &format!("{{\"from\":{from}}}"))
            .await;
        let mut frames = VecDeque::from(
            self.until(|frame| frame["id"].as_u64() == Some(request))
                .await,
        );
        let answer = frames.pop_back().expect("the attach answer");
        assert!(
            answer["result"]["committed"].is_u64(),
            "the answer names the committed record: {answer}"
        );
        frames.into()
    }
}

/// Whether `frame` is the event of `kind`.
fn is_event(frame: &Value, kind: &str) -> bool {
    frame["method"] == "event" && frame["params"]["type"] == kind
}

/// The text of the deltas `frames` carry, joined.
fn deltas(frames: &[Value]) -> String {
    let mut text = String::new();
    for frame in frames {
        if is_event(frame, "text_delta") {
            text.push_str(frame["params"]["text"].as_str().expect("a delta"));
        }
    }
    text
}

/// A worker whose run waits for an approval, the client that started it, and
/// the id of the approval.
async fn waiting(state: &Path) -> (Worker, Client, u64) {
    let cwd = state.join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    ask_folder(&cwd).await;
    let provider = script(state, APPROVAL).await;
    let worker = Worker::start(state, &cwd, &["--provider", &provider]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    let asked = a.until(|frame| frame["method"] == "approval_request").await;
    let id = asked.last().expect("an approval").clone()["params"]["id"]
        .as_u64()
        .expect("an approval id");
    (worker, a, id)
}

/// The exit code, the lines of stdout and the text of stderr of
/// `reagent rpc <session>` in `state`, with `input` on its stdin.
async fn rpc(state: &Path, session: &str, input: &str) -> (i32, Vec<Value>, String) {
    let mut child = Command::new(BIN)
        .env("XDG_STATE_HOME", state)
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_CACHE_HOME", "/nonexistent/reagent-test-cache")
        .arg("rpc")
        .arg(session)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the helper starts");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        // A helper that stopped early closes its stdin, which is no error.
        let _ = stdin.write_all(input.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("the helper exits in time")
        .expect("the helper ran");
    let mut lines = Vec::new();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(frame) => lines.push(frame),
            Err(error) => panic!("{error}: {}", String::from_utf8_lossy(line)),
        }
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().expect("an exit code");
    (code, lines, stderr)
}

/// The helper answers the approval a run waits for, and the run goes on to its
/// end.
#[tokio::test]
async fn rpc_answers_an_open_approval_and_the_run_goes_on() {
    let state = state();
    let (worker, mut a, id) = waiting(state.path()).await;
    let (code, lines, stderr) = rpc(
        state.path(),
        worker.session(),
        &format!("{{\"method\":\"approve\",\"params\":{{\"id\":{id},\"answer\":\"once\"}}}}\n"),
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0]["method"], "approval_request", "{}", lines[0]);
    assert_eq!(lines[0]["params"]["id"], id);
    assert!(lines[1]["result"].is_object(), "{}", lines[1]);
    assert_eq!(lines[2]["method"], "approval_resolved", "{}", lines[2]);
    assert_eq!(lines[2]["params"]["id"], id);

    // The run the helper answered goes on, and ends with the text of the
    // script.
    let ended = a.until(|frame| is_event(frame, "run_end")).await;
    assert_eq!(deltas(&ended), "done");
    let mut worker = worker;
    worker.kill().await;
}

/// A helper with nothing on stdin prints the request that waits, and leaves the
/// run as it was. A prefix of the session id finds it too.
#[tokio::test]
async fn rpc_with_no_input_prints_the_open_requests() {
    let state = state();
    let (worker, _a, id) = waiting(state.path()).await;
    let session = worker.session().chars().take(6).collect::<String>();
    let (code, lines, stderr) = rpc(state.path(), &session, "").await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["method"], "approval_request", "{}", lines[0]);
    assert_eq!(lines[0]["params"]["id"], id);
    let entry = worker.marked(true).await;
    assert_eq!(
        entry.state,
        reagent_protocol::discovery::State::Blocked,
        "the run waits for the person the helper printed"
    );
    assert_eq!(
        entry.wait.map(|wait| wait.id),
        Some(id),
        "the entry names the approval"
    );
    let mut worker = worker;
    worker.kill().await;
}

/// A session of no entry is named on stderr, and the exit code says the
/// arguments named nothing live.
#[tokio::test]
async fn rpc_of_an_unknown_session_exits_two() {
    let state = state();
    let (code, lines, stderr) = rpc(state.path(), "nosuchsession", "").await;
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(lines.is_empty(), "{lines:?}");
    assert!(
        stderr.contains("reagent: no live session nosuchsession"),
        "{stderr}"
    );
}
