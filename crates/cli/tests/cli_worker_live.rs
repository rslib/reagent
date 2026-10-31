//! End-to-end tests of a live worker: two clients attach to one session, catch
//! up on the frames they missed and send commands to the same run.
//!
//! Each test starts the binary against a scripted fake provider and a state
//! folder of its own, so these tests exist only with the `testing` feature.
#![cfg(feature = "testing")]

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use reagent_protocol::message::RpcError;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for a line before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// How many deltas a reply that must still be open streams.
const OPEN: usize = 3000;

/// How many replies of `OPEN` deltas a client that reads nothing falls behind
/// on: more than any socket buffer plus a full queue. The client that reads
/// asks for each one after the last, so no burst outruns it.
const ROUNDS: usize = 14;

/// A script of `ROUNDS` replies of `OPEN` deltas each, and the text of one.
fn rounds() -> (String, String) {
    let parts: Vec<String> = (0..OPEN).map(|at| format!("d{at};")).collect();
    let full = parts.concat();
    let replies: Vec<Value> = (0..ROUNDS)
        .map(|_| serde_json::json!({ "deltas": parts }))
        .collect();
    (serde_json::json!({ "replies": replies }).to_string(), full)
}

/// How many bytes one read of a test client asks for.
const CHUNK: usize = 8192;

/// The marker of the notice of a client that left.
const CLIENT_LEFT: &[u8] = br#""method":"client_left""#;

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

    async fn entry(&self) -> reagent_protocol::discovery::Entry {
        let bytes = tokio::fs::read(self.entry_path())
            .await
            .expect("the entry file is there");
        serde_json::from_slice(&bytes).expect("the entry is JSON")
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
    ///
    /// The used prefix of the buffer is dropped in one go once it is long, so a
    /// client that reads thousands of frames copies none of them.
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
                assert!(
                    self.buf.len() == self.at,
                    "the worker closed after half a frame"
                );
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

    /// Attaches from `from`, and returns the deltas of the frames it missed.
    ///
    /// This one reads the bytes for the two markers only, so that a client that
    /// catches up mid-stream does not fall behind the frames that follow.
    async fn attach_raw(&mut self, from: u64) -> String {
        let request = self
            .request("attach", &format!("{{\"from\":{from}}}"))
            .await;
        let answer = format!("\"id\":{request},");
        let mut text = String::new();
        loop {
            let Some(line) = self.line().await else {
                panic!(
                    "the worker closed the connection; read {} deltas",
                    text.len()
                );
            };
            if let Some(delta) = marker(line, br#""type":"text_delta","text":""#) {
                text.push_str(std::str::from_utf8(delta).expect("a delta is text"));
            } else if line
                .windows(answer.len())
                .any(|window| window == answer.as_bytes())
            {
                assert!(
                    line.windows(10).any(|window| window == b"\"result\":{"),
                    "the answer of the attach: {}",
                    String::from_utf8_lossy(line)
                );
                return text;
            }
        }
    }

    /// Reads to the end of the run, joining the deltas without parsing them
    /// and naming the clients that left.
    ///
    /// A client that must keep up with thousands of frames reads the bytes for
    /// the markers only.
    async fn run_end_and_left(&mut self) -> (String, String, Vec<u64>) {
        let mut text = String::new();
        let mut left = Vec::new();
        loop {
            let Some(line) = self.line().await else {
                panic!(
                    "the worker closed the connection; read {} deltas",
                    text.len()
                );
            };
            if let Some(delta) = marker(line, br#""type":"text_delta","text":""#) {
                text.push_str(std::str::from_utf8(delta).expect("a delta is text"));
            } else if let Some(reason) = marker(line, br#""type":"run_end","reason":""#) {
                return (
                    text,
                    std::str::from_utf8(reason)
                        .expect("a reason is text")
                        .to_string(),
                    left,
                );
            } else if line
                .windows(CLIENT_LEFT.len())
                .any(|window| window == CLIENT_LEFT)
                && let Some(id) = client_id(line)
            {
                left.push(id);
            }
        }
    }

    /// Detaches, and returns the answer.
    async fn detach(&mut self) -> Value {
        let request = self.request("detach", "null").await;
        self.answer(request).await
    }

    /// Reads to the end of the run, joining the deltas without parsing them.
    ///
    /// A client that must keep up with thousands of frames reads the bytes for
    /// the two markers only.
    async fn run_end(&mut self) -> (String, String) {
        let mut text = String::new();
        loop {
            let Some(line) = self.line().await else {
                panic!(
                    "the worker closed the connection; read {} deltas",
                    text.len()
                );
            };
            if let Some(delta) = marker(line, br#""type":"text_delta","text":""#) {
                text.push_str(std::str::from_utf8(delta).expect("a delta is text"));
            } else if let Some(reason) = marker(line, br#""type":"run_end","reason":""#) {
                return (
                    text,
                    std::str::from_utf8(reason)
                        .expect("a reason is text")
                        .to_string(),
                );
            }
        }
    }
}

/// The value of a frame that carries `prefix`, up to its closing quote.
///
/// `prefix` ends with the quote the value opens, and the value carries no
/// escape: a test writes its own deltas and reasons.
fn marker<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let at = line
        .windows(prefix.len())
        .position(|window| window == prefix)?;
    let rest = &line[at + prefix.len()..];
    let end = rest.iter().position(|byte| *byte == b'"')?;
    Some(&rest[..end])
}

/// The number of the `client` of a notice, which carries no quote.
fn client_id(line: &[u8]) -> Option<u64> {
    let prefix = b"\"client\":";
    let at = line
        .windows(prefix.len())
        .position(|window| window == prefix)?;
    let rest = &line[at + prefix.len()..];
    let end = rest
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

/// Whether `frame` is the event of `kind`.
fn is_event(frame: &Value, kind: &str) -> bool {
    frame["method"] == "event" && frame["params"]["type"] == kind
}

/// The event params of `frames`, in order, without the notices of clients.
fn events(frames: Vec<Value>) -> Vec<Value> {
    frames
        .into_iter()
        .filter(|frame| frame["method"] == "event")
        .map(|frame| frame["params"].clone())
        .collect()
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

/// Starts a worker whose script is `replies`, in a fresh folder.
async fn worker(state: &tempfile::TempDir, replies: &str) -> (Worker, PathBuf) {
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), replies).await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    (worker, cwd)
}

#[tokio::test]
async fn two_clients_get_one_user_message_and_the_same_events() {
    let state = state();
    let (worker, _) = worker(&state, r#"{"replies":[{"text":"done"}]}"#).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    assert_eq!(a.id, 1, "the first client is one");
    assert_eq!(b.id, 2, "the second client is two");
    a.attach(a.committed).await;
    b.attach(b.committed).await;

    let sent = a
        .request("send", r#"{"text":"hello","mode":"queue"}"#)
        .await;
    let answer = a.answer(sent).await;
    assert!(answer["result"].is_object(), "{answer}");

    let from_a = events(a.until(|frame| is_event(frame, "run_end")).await);
    let from_b = events(b.until(|frame| is_event(frame, "run_end")).await);
    assert_eq!(from_a, from_b, "both clients see the same events in order");

    let user: Vec<&Value> = from_a
        .iter()
        .filter(|event| event["type"] == "user_message")
        .collect();
    assert_eq!(user.len(), 1, "one message, one event: {from_a:?}");
    assert_eq!(
        user[0]["client"], a.id,
        "the event names the client that sent it"
    );
    assert_eq!(user[0]["text"], "hello");
    assert_eq!(user[0]["mode"], "queue");
    assert_eq!(
        from_a.first().map(|event| event["type"].clone()),
        Some(Value::from("user_message")),
        "the message opens the run"
    );
    assert_eq!(
        from_a.last().map(|event| event["type"].clone()),
        Some(Value::from("run_end")),
        "the run ends the frames"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_client_that_attaches_mid_reply_gets_it_from_its_first_delta() {
    let state = state();
    let parts: Vec<String> = (0..OPEN).map(|at| format!("d{at};")).collect();
    let full = parts.concat();
    let body = serde_json::json!({ "replies": [{ "deltas": parts }] }).to_string();
    let (worker, _) = worker(&state, &body).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    a.attach(a.committed).await;

    let sent = a.request("send", r#"{"text":"go","mode":"queue"}"#).await;
    assert!(a.answer(sent).await["result"].is_object());
    // The reply is open when the second client attaches: it reads the frames of
    // the first delta from the hub, then the ones that follow.
    let opened = a.until(|frame| is_event(frame, "text_delta")).await;
    assert_eq!(deltas(&opened).len(), "d0;".len(), "the reply has started");
    // Both clients read at once: the worker streams faster than one reader
    // parses its frames, and a client whose queue fills is closed.
    let before = tokio::spawn(async move {
        let (text, _) = a.run_end().await;
        text
    });
    let missed = b.attach_raw(b.committed).await;
    let again = b.run_end().await.0;
    let left = before.await.expect("the first client reads to the end");

    let from_a = format!("{}{}", deltas(&opened), left);
    let from_b = format!("{missed}{again}");
    assert_eq!(
        from_b, full,
        "the catch-up starts at the first delta of the open reply, with no gap and no repeat"
    );
    assert_eq!(
        from_a, full,
        "the client that was there sees the same reply"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn two_answers_to_one_approval_run_the_call_once() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    ask_folder(&cwd).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"bash","arguments":{"command":"echo hi >> ran.txt"}}]},
            {"text":"done"}]}"#,
    )
    .await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    a.attach(a.committed).await;
    b.attach(b.committed).await;

    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    let asked = a.until(|frame| frame["method"] == "approval_request").await;
    let ask = asked.last().expect("an approval").clone();
    let id = ask["params"]["id"].as_u64().expect("an approval id");
    assert_eq!(ask["params"]["tool"], "bash", "{ask}");
    let other = b.until(|frame| frame["method"] == "approval_request").await;
    assert_eq!(
        other.last().expect("the same approval"),
        &ask,
        "both clients get one request, with the same id"
    );

    let once = a
        .request("approve", &format!("{{\"id\":{id},\"answer\":\"once\"}}"))
        .await;
    assert!(
        a.answer(once).await["result"].is_object(),
        "the answer is taken"
    );
    let refuse = b
        .request(
            "approve",
            &format!("{{\"id\":{id},\"answer\":\"refuse\",\"reason\":\"no\"}}"),
        )
        .await;
    assert!(
        b.answer(refuse).await["result"].is_object(),
        "a late answer is still taken"
    );

    let from_a = a.until(|frame| is_event(frame, "run_end")).await;
    let from_b = b.until(|frame| is_event(frame, "run_end")).await;
    let resolved: Vec<&Value> = from_a
        .iter()
        .filter(|frame| frame["method"] == "approval_resolved")
        .collect();
    assert_eq!(resolved.len(), 1, "one answer decided the call: {from_a:?}");
    assert_eq!(resolved[0]["params"]["id"], id);
    assert_eq!(
        resolved[0]["params"]["client"], a.id,
        "the answer that decided names its client"
    );
    let seen_b: Vec<&Value> = from_b
        .iter()
        .filter(|frame| frame["method"] == "approval_resolved")
        .collect();
    assert_eq!(seen_b.len(), 1, "{from_b:?}");
    assert_eq!(seen_b[0], resolved[0], "both clients see the one answer");
    let ran = tokio::fs::read_to_string(cwd.join("ran.txt"))
        .await
        .expect("the call ran");
    assert_eq!(ran, "hi\n", "the call ran once");
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_client_that_attaches_while_an_approval_waits_gets_it() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    ask_folder(&cwd).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"bash","arguments":{"command":"echo hi"}}]},
            {"text":"done"}]}"#,
    )
    .await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    let asked = a.until(|frame| frame["method"] == "approval_request").await;
    let id = asked.last().expect("an approval").clone()["params"]["id"]
        .as_u64()
        .expect("an approval id");

    let mut b = Client::initialize(&worker, &token, "b").await;
    let missed = b.attach(b.committed).await;
    let caught: Vec<&Value> = missed
        .iter()
        .filter(|frame| frame["method"] == "approval_request")
        .collect();
    assert_eq!(
        caught.len(),
        1,
        "the catch-up carries the request that waits: {missed:?}"
    );
    assert_eq!(caught[0]["params"]["id"], id);

    let once = a
        .request("approve", &format!("{{\"id\":{id},\"answer\":\"once\"}}"))
        .await;
    assert!(a.answer(once).await["result"].is_object());
    let from_a = a.until(|frame| is_event(frame, "run_end")).await;
    let from_b = b.until(|frame| is_event(frame, "run_end")).await;
    for frames in [&from_a, &from_b] {
        let resolved: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["method"] == "approval_resolved")
            .collect();
        assert_eq!(resolved.len(), 1, "{frames:?}");
        assert_eq!(resolved[0]["params"]["client"], a.id);
    }
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_joined_and_left_client_reaches_the_others() {
    let state = state();
    let (worker, _) = worker(&state, r#"{"replies":[]}"#).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "alpha").await;
    a.attach(a.committed).await;
    let mut b = Client::initialize(&worker, &token, "beta").await;
    b.attach(b.committed).await;

    let joined = a
        .until(|frame| frame["method"] == "client_joined" && frame["params"]["client"] == b.id)
        .await;
    let named: Vec<(u64, String)> = joined
        .iter()
        .filter(|frame| frame["method"] == "client_joined")
        .map(|frame| {
            (
                frame["params"]["client"].as_u64().expect("a client"),
                frame["params"]["name"]
                    .as_str()
                    .expect("a name")
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(
        named,
        vec![(a.id, "alpha".to_string()), (b.id, "beta".to_string())],
        "each client that attaches is named, with the name it gave"
    );

    let answer = b.detach().await;
    assert!(answer["result"].is_object(), "{answer}");
    let left = a.until(|frame| frame["method"] == "client_left").await;
    assert_eq!(
        left.last().expect("a left notice")["params"]["client"],
        b.id,
        "the client that stays hears that the other left"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_client_that_reads_nothing_is_cut_and_the_others_go_on() {
    let state = state();
    let (body, full) = rounds();
    let (worker, _) = worker(&state, &body).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    b.attach(b.committed).await;

    // The first client never reads again: its queue fills, and the worker cuts
    // it while the second client reads every run.
    let mut left = Vec::new();
    for _ in 0..ROUNDS {
        b.request("send", r#"{"text":"go","mode":"queue"}"#).await;
        let (text, reason, gone) = b.run_end_and_left().await;
        assert_eq!(reason, "end_turn");
        assert_eq!(text, full, "the client that reads gets the whole reply");
        left.extend(gone);
    }
    assert_eq!(
        left,
        vec![a.id],
        "the client that stays hears that the other was cut"
    );
    let mut worker = worker;
    assert!(
        worker
            .child
            .try_wait()
            .expect("the worker answers")
            .is_none(),
        "the worker stays up"
    );
    worker.kill().await;
}

#[tokio::test]
async fn a_client_that_detaches_and_reads_nothing_lets_the_worker_stop() {
    let state = state();
    let (body, _) = rounds();
    let (mut worker, _) = worker(&state, &body).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    b.attach(b.committed).await;

    // One run fills the first client's socket while it reads nothing, and
    // leaves frames in its queue, but not enough to cut it. Then it leaves and
    // keeps its socket open: its writer is stuck on the full socket.
    b.request("send", r#"{"text":"go","mode":"queue"}"#).await;
    let (_, reason, _) = b.run_end_and_left().await;
    assert_eq!(reason, "end_turn");
    a.request("detach", "{}").await;
    b.request("detach", "{}").await;

    let status = tokio::time::timeout(WAIT, worker.child.wait())
        .await
        .expect("the worker stops though a client stopped reading")
        .expect("the worker is waited for");
    assert_eq!(status.code(), Some(0));
    drop(a);
}

#[tokio::test]
async fn a_cancel_mid_stream_ends_the_run_and_the_worker_stays_up() {
    let state = state();
    let parts: Vec<String> = (0..OPEN).map(|at| format!("d{at};")).collect();
    let body =
        serde_json::json!({ "replies": [{ "deltas": parts }, { "text": "after" }] }).to_string();
    let (worker, _) = worker(&state, &body).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"go","mode":"queue"}"#).await;
    let opened = a.until(|frame| is_event(frame, "text_delta")).await;
    assert!(!deltas(&opened).is_empty(), "the reply is streaming");

    a.request("cancel", "null").await;
    let (_, reason) = a.run_end().await;
    assert_eq!(reason, "cancelled", "the cancel stops the run that is open");

    // The worker stays up: a message after the cancel starts the next run.
    a.request("send", r#"{"text":"hello","mode":"queue"}"#)
        .await;
    let (text, reason) = a.run_end().await;
    assert_eq!(reason, "end_turn");
    assert_eq!(text, "after", "the next run streams its own reply");
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_set_with_a_mode_that_does_not_parse_is_invalid_params() {
    let state = state();
    let (worker, _) = worker(&state, r#"{"replies":[]}"#).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let bad = a.request("set", r#"{"mode":"nope"}"#).await;
    let answer = a.answer(bad).await;
    assert_eq!(
        answer["error"]["code"],
        RpcError::INVALID_PARAMS,
        "{answer}"
    );
    assert!(
        a.frames.iter().all(|frame| frame["method"] != "event"),
        "nothing reached the session: {:?}",
        a.frames
    );

    let good = a.request("set", r#"{"mode":"auto"}"#).await;
    assert!(
        a.answer(good).await["result"].is_object(),
        "a mode that parses is taken"
    );
    let mut worker = worker;
    worker.kill().await;
}

/// Starts a worker in `cwd` whose script is the file `name` of `state` and
/// holds `replies`, in a project whose config asks for every `bash` call.
async fn start_worker(state: &Path, cwd: &Path, name: &str, replies: &str) -> Worker {
    // A second session of the project finds its config already there.
    if tokio::fs::metadata(cwd.join(".git")).await.is_err() {
        ask_folder(cwd).await;
    }
    let path = state.join(name);
    tokio::fs::write(&path, replies)
        .await
        .expect("writes the script");
    Worker::start(
        state,
        cwd,
        &["--provider", &format!("fake:{}", path.display())],
    )
    .await
}

/// The saved-rules file of the project of `cwd`, once the worker writes it:
/// `<XDG_STATE_HOME>/reagent/permissions/<key>.toml`, outside the repo.
async fn saved_rules_file(state: &Path, cwd: &Path) -> PathBuf {
    // The worker runs in the folder as the system resolves it, which is not
    // the `/tmp` symlink of the path a test writes.
    let cwd = tokio::fs::canonicalize(cwd)
        .await
        .expect("the project folder resolves");
    let folder = state.join("reagent");
    let permissions = folder.join("permissions");
    let file = reagent_config::saved::path(&folder, &cwd);
    assert_eq!(
        file.parent(),
        Some(permissions.as_path()),
        "the rules live in the state folder, not in the project"
    );
    tokio::time::timeout(WAIT, async {
        while tokio::fs::metadata(&file).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the saved rules in time: {}", file.display()));
    file
}

/// Runs one scripted `bash` call `command` in `cwd`, answers its approval with
/// `save`, and returns the saved-rules file once it holds the rule the request
/// named.
async fn save_one(state: &Path, cwd: &Path, name: &str, command: &str) -> PathBuf {
    let replies = serde_json::json!({ "replies": [
        { "calls": [{ "name": "bash", "arguments": { "command": command } }] },
        { "text": "done" },
    ]})
    .to_string();
    let mut worker = start_worker(state, cwd, name, &replies).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;

    let asked = a.until(|frame| frame["method"] == "approval_request").await;
    let ask = asked.last().expect("an approval").clone();
    assert_eq!(ask["params"]["tool"], "bash", "{ask}");
    let rule = ask["params"]["save"][0]
        .as_str()
        .expect("the rule a save would add")
        .to_string();
    let id = ask["params"]["id"].as_u64().expect("an approval id");
    let saved = a
        .request("approve", &format!("{{\"id\":{id},\"answer\":\"save\"}}"))
        .await;
    assert!(
        a.answer(saved).await["result"].is_object(),
        "the save is taken"
    );
    let (text, reason) = a.run_end().await;
    assert_eq!(reason, "end_turn", "the run goes on after the save");
    assert_eq!(text, "done");

    let file = saved_rules_file(state, cwd).await;
    let body = tokio::fs::read_to_string(&file)
        .await
        .expect("the saved rules read");
    assert!(
        body.contains(&format!("{rule:?}")),
        "the file holds the rule the request named: {body}"
    );
    worker.kill().await;
    file
}

#[tokio::test]
async fn a_saved_rule_outlives_the_session() {
    use std::os::unix::fs::PermissionsExt;

    let state = state();
    let cwd = state.path().join("p");
    let file = save_one(state.path(), &cwd, "one.json", "echo hi one").await;
    let mode = tokio::fs::metadata(&file)
        .await
        .expect("the saved rules mode")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "only its owner may read the saved rules");
    assert!(
        !cwd.join(".reagent").join("permissions.toml").exists(),
        "nothing of the save goes into the repo"
    );

    // A new session of the same project runs the next call without asking.
    let replies = serde_json::json!({ "replies": [
        { "calls": [{ "name": "bash", "arguments": { "command": "echo hi two" } }] },
        { "text": "again" },
    ]})
    .to_string();
    let mut worker = start_worker(state.path(), &cwd, "two.json", &replies).await;
    let token = worker.entry().await.token;
    let mut b = Client::initialize(&worker, &token, "b").await;
    b.attach(b.committed).await;
    b.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;

    let frames = b.until(|frame| is_event(frame, "run_end")).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "approval_request"),
        "the saved rule answers the next session: {frames:?}"
    );
    assert_eq!(
        events(frames).last().expect("the run ends").get("reason"),
        Some(&Value::from("end_turn")),
        "the run ends as a run that ran its call"
    );
    worker.kill().await;
}

#[tokio::test]
async fn a_saved_rule_does_not_reach_another_project() {
    let state = state();
    let p = state.path().join("p");
    save_one(state.path(), &p, "one.json", "echo hi one").await;

    let q = state.path().join("q");
    let replies = serde_json::json!({ "replies": [
        { "calls": [{ "name": "bash", "arguments": { "command": "echo hi two" } }] },
        { "text": "done" },
    ]})
    .to_string();
    let mut worker = start_worker(state.path(), &q, "two.json", &replies).await;
    let token = worker.entry().await.token;
    let mut c = Client::initialize(&worker, &token, "c").await;
    c.attach(c.committed).await;
    c.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;

    let asked = c.until(|frame| frame["method"] == "approval_request").await;
    let ask = asked.last().expect("an approval").clone();
    assert!(
        ask["params"]["call"]
            .as_str()
            .is_some_and(|call| call.contains("echo hi two")),
        "another project asks for its own call: {ask}"
    );
    let id = ask["params"]["id"].as_u64().expect("an approval id");
    let once = c
        .request("approve", &format!("{{\"id\":{id},\"answer\":\"once\"}}"))
        .await;
    assert!(c.answer(once).await["result"].is_object());
    let (_, reason) = c.run_end().await;
    assert_eq!(reason, "end_turn");
    worker.kill().await;
}
