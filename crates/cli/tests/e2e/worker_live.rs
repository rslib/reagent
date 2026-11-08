//! End-to-end tests of a live worker: two clients attach to one session, catch
//! up on the frames they missed and send commands to the same run.
//!
//! Each test starts the binary against a scripted fake provider and a state
//! folder of its own, so these tests exist only with the `testing` feature.
#![cfg(feature = "testing")]

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reagent_protocol::message::RpcError;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::mpsc;

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
    wire(dir, "script.json", body).await
}

/// A scripted provider reply written to `name` of `dir`, and the wire value
/// that reads it.
async fn wire(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    tokio::fs::write(&path, body)
        .await
        .expect("writes the script");
    format!("fake:{}", path.display())
}

/// A config folder whose `[providers]` tables are `body`, and that folder.
async fn config_of(dir: &Path, body: &str) -> PathBuf {
    let config = dir.join("config");
    tokio::fs::create_dir_all(config.join("reagent"))
        .await
        .expect("the config folder");
    tokio::fs::write(config.join("reagent").join("config.toml"), body)
        .await
        .expect("the config");
    config
}

/// The tables of the providers `one` and `two`, each with the keys `one` and
/// `two` hold.
fn providers(one: &str, two: &str) -> String {
    format!("[providers.one]\n{one}\n\n[providers.two]\n{two}\n")
}

/// The models cache folder of `dir`, whose providers `models` lists as the
/// provider name and the entries of its cache file.
async fn cache_of(dir: &Path, models: &[(&str, &str)]) -> PathBuf {
    let cache = dir.join("cache");
    let folder = cache.join("reagent").join("models");
    tokio::fs::create_dir_all(&folder)
        .await
        .expect("the cache folder");
    for (provider, entries) in models {
        tokio::fs::write(
            folder.join(format!("{provider}.json")),
            format!("{{\"fetched_at\":1,\"models\":[{entries}]}}"),
        )
        .await
        .expect("the cache file");
    }
    cache
}

/// A reply that ends the turn with one short text, as the Anthropic wire
/// streams it.
const HTTP_REPLY: &str = concat!(
    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);

/// A reply that calls the tool `name` with the JSON `arguments` and ends the
/// turn with the tool use.
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

/// A reply that calls `read` on `path` and ends the turn with the tool use.
fn read_reply(path: &str) -> String {
    let arguments = serde_json::json!({ "path": path }).to_string();
    call_reply("read", &arguments)
}

/// A reply that calls `bash` with `command` and ends the turn with the tool
/// use, which waits for an approval in a folder that asks for one.
fn bash_reply(command: &str) -> String {
    call_reply(
        "bash",
        &serde_json::json!({ "command": command }).to_string(),
    )
}

/// A 64x48 RGB PNG, made here so the test holds the exact bytes it reads.
fn shot_png() -> Vec<u8> {
    let (width, height) = (64u32, 48u32);
    let data: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).flat_map(move |x| [(x * 4) as u8, (y * 5) as u8, 90]))
        .collect();
    let mut out = Vec::new();
    {
        let mut info = png::Info::with_size(width, height);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Eight;
        let mut encoder = png::Encoder::with_info(&mut out, info).expect("png info");
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&data).expect("png data");
        writer.finish().expect("png finish");
    }
    out
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
    async fn body(&mut self) -> Value {
        let body = self.bodies.recv().await.expect("a recorded request");
        serde_json::from_str(&body).expect("a JSON body")
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

/// The tables of two providers that both answer through `base` on the
/// Anthropic wire, and the config folder that holds them, which only the user
/// may read since it holds a key.
async fn anthropic_config(dir: &Path, base: &str) -> PathBuf {
    let table = format!("wire = \"anthropic\"\nbase_url = \"{base}\"\napi_key = \"sk-test\"");
    let config = config_of(dir, &providers(&table, &table)).await;
    tokio::fs::set_permissions(
        config.join("reagent").join("config.toml"),
        std::fs::Permissions::from_mode(0o600),
    )
    .await
    .expect("the config is private");
    config
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
        Worker::start_with(
            state,
            cwd,
            Path::new("/nonexistent/reagent-test-config"),
            args,
        )
        .await
    }

    /// Starts a worker that reads the config folder `config`, so a model table
    /// of it names the context window of a model.
    async fn start_with(state: &Path, cwd: &Path, config: &Path, args: &[&str]) -> Worker {
        Worker::start_with_cache(
            state,
            cwd,
            config,
            Path::new("/nonexistent/reagent-test-cache"),
            args,
        )
        .await
    }

    /// Starts a worker that reads the models cache folder `cache`, so a model
    /// that cache lists carries the window and the limits it names.
    async fn start_with_cache(
        state: &Path,
        cwd: &Path,
        config: &Path,
        cache: &Path,
        args: &[&str],
    ) -> Worker {
        let mut child = Command::new(BIN)
            .env("XDG_CONFIG_HOME", config)
            .env("XDG_CACHE_HOME", cache)
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

    /// Stops the worker with SIGTERM, so it closes its journal on the way out.
    async fn term(&mut self) {
        let pid = self.child.id().expect("a running worker").to_string();
        let sent = tokio::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .await
            .expect("kill runs");
        assert!(sent.success());
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
    /// Whether the answer to the last `attach` said a run is open.
    running: bool,
    /// The model the answer to `initialize` named.
    model: String,
    /// The context window the answer to `initialize` named.
    window: Option<u64>,
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
            running: false,
            model: String::new(),
            window: None,
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
        client.model = answer["result"]["model"]
            .as_str()
            .unwrap_or_else(|| panic!("the answer names the model: {answer}"))
            .to_string();
        client.window = answer["result"]["context_window"].as_u64();
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
        self.running = answer["result"]["running"]
            .as_bool()
            .unwrap_or_else(|| panic!("the answer says whether a run is open: {answer}"));
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

/// A session a client started has no task, so its first message names it.
#[tokio::test]
async fn session_title_is_the_first_message_a_client_sends() {
    let state = state();
    let (worker, _) = worker(&state, r#"{"replies":[{"text":"done"}]}"#).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;
    let sent = client
        .request("send", r#"{"text":"name this session","mode":"queue"}"#)
        .await;
    client.answer(sent).await;
    client.until(|frame| is_event(frame, "run_end")).await;

    let root = state.path().join("reagent/sessions");
    let mut found = false;
    for _ in 0..500 {
        let mut folders = tokio::fs::read_dir(&root).await.expect("the sessions");
        while let Some(folder) = folders.next_entry().await.expect("a folder") {
            let list = tokio::fs::read(folder.path().join("sessions.list"))
                .await
                .unwrap_or_default();
            found |= list
                .windows(b"name this session".len())
                .any(|part| part == b"name this session");
        }
        if found {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(found, "the list entry takes the first message as its title");
}

#[tokio::test]
async fn an_attach_after_the_run_ended_says_no_run_is_open() {
    let state = state();
    let (worker, _) = worker(&state, r#"{"replies":[{"text":"done"}]}"#).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    a.attach(a.committed).await;
    let sent = a
        .request("send", r#"{"text":"hello","mode":"queue"}"#)
        .await;
    assert!(a.answer(sent).await["result"].is_object());
    // The run ends, and the record that holds it commits before its `run_end`
    // goes out: `run_end` is the last frame of the run.
    let seen = events(a.until(|frame| is_event(frame, "run_end")).await);
    assert!(
        seen.iter().any(|event| event["type"] == "committed"),
        "the record of the run committed: {seen:?}"
    );

    b.attach(b.committed).await;
    assert!(!b.running, "the answer says no run is open once one ended");
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn an_attach_during_a_run_says_a_run_is_open() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    ask_folder(&cwd).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"one"},
            {"calls":[{"name":"bash","arguments":{"command":"echo hi"}}]},
            {"text":"done"}]}"#,
    )
    .await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    // A first run ends, so its `run_end` goes out before the record that holds
    // it commits.
    let sent = a
        .request("send", r#"{"text":"first","mode":"queue"}"#)
        .await;
    assert!(a.answer(sent).await["result"].is_object());
    a.until(|frame| is_event(frame, "run_end")).await;

    // The next run commits before it asks, which ends what a client catches up
    // on: the `run_end` of the first run is in no frame at all. This one waits
    // for the approval of its call, so it is open when the second client
    // attaches.
    let sent = a
        .request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    assert!(a.answer(sent).await["result"].is_object());
    let opened = a.until(|frame| frame["method"] == "approval_request").await;

    let mut b = Client::initialize(&worker, &token, "b").await;
    let missed = b.attach(b.committed).await;
    assert!(b.running, "the run that waits for the approval is open");
    assert!(
        !missed.iter().any(|frame| is_event(frame, "run_end")),
        "the catch-up holds no `run_end` of the run that ended: {missed:?}"
    );
    assert!(
        missed
            .iter()
            .any(|frame| frame["method"] == "approval_request"),
        "the catch-up holds the request that waits: {missed:?} / {opened:?}"
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

#[tokio::test]
async fn initialize_names_the_model_of_the_run_and_its_context_window() {
    let state = state();
    // A model table names the context window of the model of `--model`.
    let config = state.path().join("config");
    tokio::fs::create_dir_all(config.join("reagent"))
        .await
        .expect("the config folder");
    tokio::fs::write(
        config.join("reagent").join("config.toml"),
        "[providers.fi.models.\"m\"]\ncontext_window = 200000\n",
    )
    .await
    .expect("the config");
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies":[]}"#).await;
    let worker = Worker::start_with(
        state.path(),
        &cwd,
        &config,
        &["--provider", &provider, "--model", "fi/m"],
    )
    .await;
    let token = worker.entry().await.token;
    let a = Client::initialize(&worker, &token, "a").await;
    assert_eq!(
        a.model, "fi/m",
        "initialize names the provider and the model of the run"
    );
    assert_eq!(
        a.window,
        Some(200_000),
        "initialize names the context window of the model"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_set_of_another_model_sends_one_settings_notice_to_two_clients() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies":[]}"#).await;
    let worker = Worker::start(
        state.path(),
        &cwd,
        &["--provider", &provider, "--model", "m1"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    a.attach(a.committed).await;
    b.attach(b.committed).await;

    let set = a.request("set", r#"{"model":"m2"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session takes the model"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(notice["params"]["model"], "m2", "{notice}");
    assert_eq!(
        notice["params"]["context_window"],
        Value::Null,
        "no cache names a window for m2: {notice}"
    );
    assert_eq!(notice["params"]["level"], Value::Null, "{notice}");
    assert_eq!(notice["params"]["mode"], "default", "{notice}");
    assert!(
        seen.iter().all(|frame| frame["method"] != "event"),
        "a set sends no event: {seen:?}"
    );

    let from_b = b.until(|frame| frame["method"] == "settings").await;
    assert_eq!(
        from_b.last().expect("a settings notice"),
        notice,
        "every client gets the same notice"
    );
    // One notice per change, not one per client or per frame.
    let again = tokio::time::timeout(
        Duration::from_millis(200),
        a.until(|frame| frame["method"] == "settings"),
    )
    .await;
    assert!(again.is_err(), "one notice, not two: {again:?}");

    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_set_of_the_model_of_a_named_provider_sends_no_notice_until_another_one() {
    let state = state();
    // A model table names the context window of the model of `--model`.
    let config = state.path().join("config");
    tokio::fs::create_dir_all(config.join("reagent"))
        .await
        .expect("the config folder");
    tokio::fs::write(
        config.join("reagent").join("config.toml"),
        "[providers.fi.models.\"m\"]\ncontext_window = 200000\n",
    )
    .await
    .expect("the config");
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies":[]}"#).await;
    let worker = Worker::start_with(
        state.path(),
        &cwd,
        &config,
        &["--provider", &provider, "--model", "fi/m"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    assert_eq!(a.model, "fi/m", "the run names the provider and the model");
    a.attach(a.committed).await;

    // The same model, sent as the bare model id a client sets: no change.
    let same = a.request("set", r#"{"model":"m"}"#).await;
    assert!(
        a.answer(same).await["result"].is_object(),
        "the session takes the model"
    );
    let quiet = tokio::time::timeout(
        Duration::from_millis(200),
        a.until(|frame| frame["method"] == "settings"),
    )
    .await;
    assert!(
        quiet.is_err(),
        "the model that was set is the model of the run: {quiet:?}"
    );

    // Another model under the same provider: one notice, naming the provider.
    let other = a.request("set", r#"{"model":"other"}"#).await;
    assert!(
        a.answer(other).await["result"].is_object(),
        "the session takes the other model"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(
        notice["params"]["model"], "fi/other",
        "the notice names the provider and the model it was set to: {notice}"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn provider_switch_sends_the_next_turn_to_the_other_provider() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let one = wire(
        state.path(),
        "one.json",
        r#"{"replies":[{"text":"from one"}]}"#,
    )
    .await;
    let two = wire(
        state.path(),
        "two.json",
        r#"{"replies":[{"text":"from two"}]}"#,
    )
    .await;
    let config = config_of(
        state.path(),
        &providers(&format!("wire = \"{one}\""), &format!("wire = \"{two}\"")),
    )
    .await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    assert_eq!(a.model, "one/model-a", "the run starts on one");
    a.attach(a.committed).await;

    let set = a.request("set", r#"{"model":"two/model-b"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session switches to two"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(
        notice["params"]["model"], "two/model-b",
        "the notice names the provider and the model it switched to: {notice}"
    );

    let sent = a.request("send", r#"{"text":"go","mode":"queue"}"#).await;
    assert!(a.answer(sent).await["result"].is_object());
    let from_a = a.until(|frame| is_event(frame, "run_end")).await;
    assert_eq!(
        deltas(&from_a),
        "from two",
        "the next turn goes to the provider it switched to"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn provider_switch_on_the_same_provider_changes_the_model_only() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let one = wire(
        state.path(),
        "one.json",
        r#"{"replies":[{"text":"from one"}]}"#,
    )
    .await;
    let two = wire(
        state.path(),
        "two.json",
        r#"{"replies":[{"text":"from two"}]}"#,
    )
    .await;
    let config = config_of(
        state.path(),
        &providers(&format!("wire = \"{one}\""), &format!("wire = \"{two}\"")),
    )
    .await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let set = a.request("set", r#"{"model":"one/model-b"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session takes the model of the provider it runs"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(
        notice["params"]["model"], "one/model-b",
        "the notice names the provider it runs and the new model: {notice}"
    );

    let sent = a.request("send", r#"{"text":"go","mode":"queue"}"#).await;
    assert!(a.answer(sent).await["result"].is_object());
    let from_a = a.until(|frame| is_event(frame, "run_end")).await;
    assert_eq!(
        deltas(&from_a),
        "from one",
        "the turn stays on the provider of the run"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn switch_limits_send_the_new_window_budget() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let one = wire(
        state.path(),
        "one.json",
        r#"{"replies":[{"text":"from one"}]}"#,
    )
    .await;
    let two = wire(
        state.path(),
        "two.json",
        r#"{"replies":[{"text":"from two"}]}"#,
    )
    .await;
    let config = config_of(
        state.path(),
        &providers(&format!("wire = \"{one}\""), &format!("wire = \"{two}\"")),
    )
    .await;
    // The models cache names the window of each provider's model, and the
    // window of the one the switch names is smaller.
    let cache = cache_of(
        state.path(),
        &[
            ("one", r#"{"id":"model-a","context_window":200000}"#),
            ("two", r#"{"id":"model-b","context_window":32000}"#),
        ],
    )
    .await;
    let worker = Worker::start_with_cache(
        state.path(),
        &cwd,
        &config,
        &cache,
        &["--model", "one/model-a"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    assert_eq!(
        a.window,
        Some(200_000),
        "the run reads the window of its model from the cache"
    );
    a.attach(a.committed).await;

    // The budget follows the window, and the notice it names is what a client
    // reads: the settings the worker builds from that window are the unit test
    // `switch_limits_take_the_limits_of_the_new_model`.
    let set = a.request("set", r#"{"model":"two/model-b"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session switches to two"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(notice["params"]["model"], "two/model-b", "{notice}");
    assert_eq!(
        notice["params"]["context_window"], 32_000,
        "the notice names the window of the model it switched to: {notice}"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn switch_limits_map_the_level_for_the_new_model() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let one = wire(state.path(), "one.json", r#"{"replies":[]}"#).await;
    let two = wire(state.path(), "two.json", r#"{"replies":[]}"#).await;
    let config = config_of(
        state.path(),
        &providers(&format!("wire = \"{one}\""), &format!("wire = \"{two}\"")),
    )
    .await;
    // The model of `two` takes `low` and `high` only, so `medium` becomes the
    // next level it takes.
    let cache = cache_of(
        state.path(),
        &[
            ("one", r#"{"id":"model-a","context_window":200000}"#),
            (
                "two",
                r#"{"id":"model-b","context_window":200000,"reasoning":{"levels":["low","high"]}}"#,
            ),
        ],
    )
    .await;
    let worker = Worker::start_with_cache(
        state.path(),
        &cwd,
        &config,
        &cache,
        &["--model", "one/model-a", "--reasoning", "medium"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let set = a.request("set", r#"{"model":"two/model-b"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session switches to two"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(
        notice["params"]["level"], "high",
        "the level the new model takes is the one the session keeps: {notice}"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn switch_limits_turn_images_off_for_a_text_model() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let png = shot_png();
    tokio::fs::write(cwd.join("shot.png"), &png)
        .await
        .expect("the image");
    // The first turn reads the image, so the tool result of it is in the
    // history of every later request.
    let mut server = serve(vec![
        read_reply("shot.png"),
        HTTP_REPLY.to_string(),
        HTTP_REPLY.to_string(),
    ])
    .await;
    let config = anthropic_config(state.path(), &server.base).await;
    // The model of `two` takes no images, so the request built after the
    // switch carries the note a wire writes in their place.
    let cache = cache_of(
        state.path(),
        &[
            ("one", r#"{"id":"model-a","context_window":200000}"#),
            (
                "two",
                r#"{"id":"model-b","context_window":200000,"images":false}"#,
            ),
        ],
    )
    .await;
    let worker = Worker::start_with_cache(
        state.path(),
        &cwd,
        &config,
        &cache,
        &["--model", "one/model-a"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let sent = a.request("send", r#"{"text":"look","mode":"queue"}"#).await;
    assert!(a.answer(sent).await["result"].is_object());
    a.until(|frame| is_event(frame, "run_end")).await;
    let _tool_use = server.body().await;
    let before = server.body().await;
    assert!(
        before.to_string().contains(&STANDARD.encode(&png)),
        "the model of one takes images: {before}"
    );

    let set = a.request("set", r#"{"model":"two/model-b"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session switches to two"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(notice["params"]["model"], "two/model-b", "{notice}");
    let sent = a
        .request("send", r#"{"text":"again","mode":"queue"}"#)
        .await;
    assert!(a.answer(sent).await["result"].is_object());
    a.until(|frame| is_event(frame, "run_end")).await;
    let after = server.body().await;
    let body = after.to_string();
    assert!(
        !body.contains(&STANDARD.encode(&png)),
        "no image reaches a model that takes none: {after}"
    );
    assert!(
        body.contains("[image not sent: the model takes no images]"),
        "the wire says an image was not sent: {after}"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn provider_switch_without_a_key_answers_an_error_and_keeps_the_provider() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let one = wire(
        state.path(),
        "one.json",
        r#"{"replies":[{"text":"from one"}]}"#,
    )
    .await;
    // The provider the switch names has no key: its wire reads one variable,
    // which this test never sets.
    let config = config_of(
        state.path(),
        &providers(
            &format!("wire = \"{one}\""),
            "wire = \"anthropic\"\nbase_url = \"http://127.0.0.1:1\"\n\
             api_key_env = \"REAGENT_TEST_PROVIDER_SWITCH_KEY\"",
        ),
    )
    .await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let set = a.request("set", r#"{"model":"two/model-b"}"#).await;
    let answer = a.answer(set).await;
    assert_eq!(
        answer["error"]["code"],
        RpcError::INVALID_PARAMS,
        "a switch that fails is an error: {answer}"
    );
    let message = answer["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("an error message: {answer}"));
    assert!(
        message.contains("two"),
        "the error names the provider: {answer}"
    );
    assert!(
        message.contains("REAGENT_TEST_PROVIDER_SWITCH_KEY"),
        "the error names the reason: {answer}"
    );
    assert!(
        a.frames
            .iter()
            .all(|frame| frame["method"] != "event" && frame["method"] != "settings"),
        "a failed switch reaches the session: {:?}",
        a.frames
    );

    let sent = a.request("send", r#"{"text":"go","mode":"queue"}"#).await;
    assert!(a.answer(sent).await["result"].is_object());
    let from_a = a.until(|frame| is_event(frame, "run_end")).await;
    assert_eq!(
        deltas(&from_a),
        "from one",
        "the next turn stays on the provider of the run"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_settings_notice_names_the_mode_the_session_kept() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies":[]}"#).await;
    let worker = Worker::start(
        state.path(),
        &cwd,
        &["--provider", &provider, "--model", "m1"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    // The run did not enable `bypass`, so the session refuses the mode and
    // keeps the one it had.
    let asked = a.request("set", r#"{"mode":"bypass"}"#).await;
    assert!(
        a.answer(asked).await["result"].is_object(),
        "the command is taken"
    );
    let refused = a.until(|frame| is_event(frame, "error")).await;
    let error = refused.last().expect("an error event");
    assert!(
        error["params"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("bypass")),
        "the session refuses the mode: {error}"
    );

    let set = a.request("set", r#"{"model":"m2"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session takes the model"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(notice["params"]["model"], "m2", "{notice}");
    assert_eq!(
        notice["params"]["mode"], "default",
        "the notice names the mode the session kept, not the one it refused: {notice}"
    );
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_settings_notice_names_the_mode_a_set_was_accepted_with() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies":[]}"#).await;
    let worker = Worker::start(
        state.path(),
        &cwd,
        &["--provider", &provider, "--model", "m1"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    // The run did not enable `bypass`, so the guard of the session takes this
    // mode and the session keeps it: the notice names the mode it kept, not the
    // one the run started with.
    let mode = a.request("set", r#"{"mode":"accept-edits"}"#).await;
    assert!(
        a.answer(mode).await["result"].is_object(),
        "the command is taken"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(
        notice["params"]["mode"], "accept-edits",
        "the notice names the mode the guard took: {notice}"
    );
    assert_eq!(
        notice["params"]["model"], "m1",
        "nothing changed the model: {notice}"
    );
    assert!(
        seen.iter().all(|frame| frame["method"] != "event"),
        "the event behind the notice is not a frame of its own: {seen:?}"
    );

    // A later `set` of the model names the mode the session took before it.
    let set = a.request("set", r#"{"model":"m2"}"#).await;
    assert!(
        a.answer(set).await["result"].is_object(),
        "the session takes the model"
    );
    let seen = a.until(|frame| frame["method"] == "settings").await;
    let notice = seen.last().expect("a settings notice");
    assert_eq!(notice["params"]["model"], "m2", "{notice}");
    assert_eq!(
        notice["params"]["mode"], "accept-edits",
        "the notice names the mode in force, not the one the run started with: {notice}"
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

/// The bytes the worker keeps of the output of one call.
const TAIL: usize = 4096;

/// How many lines the scripted call prints: more bytes than the worker keeps.
const PRINTED: usize = 300;

/// The line the scripted call prints at `at`.
fn printed(at: usize) -> String {
    format!("line {at} of a long output\n")
}

/// A script of one `bash` call that prints [`PRINTED`] lines, then ends the
/// turn.
fn long_output() -> String {
    let command = format!(
        "awk 'BEGIN {{ for (i = 1; i <= {PRINTED}; i++) printf \"line %d of a long output\\n\", i }}'"
    );
    serde_json::json!({ "replies": [
        { "calls": [{ "name": "bash", "arguments": { "command": command } }] },
        { "text": "done" },
    ]})
    .to_string()
}

/// The `events` frame of `type`, which there must be one of.
fn only<'a>(seen: &'a [Value], kind: &str) -> &'a Value {
    let found: Vec<&Value> = seen.iter().filter(|event| event["type"] == kind).collect();
    assert_eq!(found.len(), 1, "one {kind} event in {seen:?}");
    found[0]
}

#[tokio::test]
async fn a_tool_result_carries_its_output_tail() {
    let state = state();
    let (mut worker, _) = worker(&state, &long_output()).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;

    let seen = events(a.until(|frame| is_event(frame, "run_end")).await);
    let result = only(&seen, "tool_result");
    let output = result["output"]
        .as_str()
        .unwrap_or_else(|| panic!("the result carries its output tail: {result}"));
    assert!(
        output.len() <= TAIL,
        "{} bytes is more than the {TAIL} the worker keeps",
        output.len()
    );
    assert!(
        output
            .lines()
            .all(|line| line.starts_with("line ") || line.starts_with("exit code: ")),
        "the tail starts at a line: {:?}",
        &output[..output.len().min(40)]
    );
    let last = output.lines().rev().find(|line| line.starts_with("line "));
    assert_eq!(
        last,
        Some(printed(PRINTED).trim_end()),
        "the tail ends with the last line the call printed"
    );
    let earlier = result["earlier"]
        .as_u64()
        .unwrap_or_else(|| panic!("the result counts the lines before the tail: {result}"));
    assert!(earlier > 0, "the cut takes lines off: {result}");
    let bytes = result["bytes"].as_u64().expect("the size of the output");
    assert!(
        bytes > TAIL as u64,
        "the output is longer than the tail it was cut to: {bytes}"
    );
    assert!(
        earlier < bytes,
        "the earlier count is lines, not bytes: {result}"
    );
    worker.kill().await;
}

#[tokio::test]
async fn a_print_run_writes_no_output_tail() {
    let tmp = tempfile::Builder::new()
        .prefix("rw")
        .tempdir_in("/tmp")
        .expect("a temp folder");
    let provider = script(
        tmp.path(),
        r#"{"replies":[{"calls":[{"name":"read","arguments":{"path":"notes.txt"}}]},
            {"text":"done"}]}"#,
    )
    .await;
    tokio::fs::write(tmp.path().join("notes.txt"), "the notes\n")
        .await
        .expect("the notes");
    let run = Command::new(BIN)
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .args(["-p", "read it", "--json", "--provider", &provider])
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    assert!(
        run.status.success(),
        "exit {}: {}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    let lines: Vec<Value> = String::from_utf8_lossy(&run.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("one JSON object a line"))
        .collect();
    let result = only(&lines, "tool_result");
    assert!(
        result.get("output").is_none() && result.get("earlier").is_none(),
        "a printed line carries no tail: {result}"
    );
    assert!(
        result["bytes"].as_u64().is_some_and(|bytes| bytes > 0),
        "the printed line names the size of the output: {result}"
    );
}

/// The text of block `block` of the first user message of an Anthropic body.
fn first_user_text(body: &Value, block: usize) -> String {
    body["messages"][0]["content"][block]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("block {block} of the first user message: {body}"))
        .to_owned()
}

/// The content blocks of the message of `body` at `at`.
fn message_blocks(body: &Value, at: usize) -> Vec<Value> {
    let content = body["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("no messages: {body}"))
        .get(at)
        .unwrap_or_else(|| panic!("no message {at}: {body}"))["content"]
        .clone();
    match content {
        Value::Array(blocks) => blocks,
        Value::String(text) => vec![serde_json::json!({ "type": "text", "text": text })],
        other => panic!("no content of message {at}: {other}"),
    }
}

/// Sends the first message and three of 40 KB, so the oldest turns lie
/// before the tail a compaction keeps.
async fn long_session(a: &mut Client) {
    send_and_wait(a, "task").await;
    let chunk = "x".repeat(40_000);
    for _ in 0..3 {
        send_and_wait(a, &chunk).await;
    }
}

/// Sends `text` to `a` and waits for the run to end.
async fn send_and_wait(a: &mut Client, text: &str) {
    let params = serde_json::json!({ "text": text, "mode": "queue" }).to_string();
    let sent = a.request("send", &params).await;
    assert!(a.answer(sent).await["result"].is_object());
    a.until(|frame| is_event(frame, "run_end")).await;
}

#[tokio::test]
async fn compact_request_puts_the_note_in_the_next_request() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let mut server = serve(vec![HTTP_REPLY.to_string(); 5]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    long_session(&mut a).await;
    for _ in 0..4 {
        let _ = server.body().await;
    }

    let asked = a
        .request("compact", r#"{"note":"keep the API notes"}"#)
        .await;
    assert!(
        a.answer(asked).await["result"].is_object(),
        "the session takes the request"
    );
    let seen = a.until(|frame| is_event(frame, "compacted")).await;
    let line = seen.last().expect("a compacted line");
    assert_eq!(
        line["params"]["replaced"], 2,
        "the first reply and the first 40 KB message became the checkpoint: {line}"
    );

    send_and_wait(&mut a, "more").await;
    let mut worker = worker;
    worker.kill().await;

    let body = server.body().await;
    assert_eq!(first_user_text(&body, 0), "task");
    let checkpoint = first_user_text(&body, 1);
    assert!(checkpoint.contains("## Checkpoint 1"), "{checkpoint}");
    assert!(
        checkpoint.ends_with("\n\nNote from the user: keep the API notes"),
        "{checkpoint}"
    );
}

/// `committed` means the records are in the part file, so a SIGKILL right after
/// it keeps the compaction it names.
#[tokio::test]
async fn a_kill_after_committed_keeps_the_compaction() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let mut server = serve(vec![HTTP_REPLY.to_string(); 5]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let mut worker =
        Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    long_session(&mut a).await;
    for _ in 0..4 {
        let _ = server.body().await;
    }
    let asked = a
        .request("compact", r#"{"note":"keep the API notes"}"#)
        .await;
    assert!(a.answer(asked).await["result"].is_object());
    a.until(|frame| is_event(frame, "compacted")).await;
    a.until(|frame| is_event(frame, "committed")).await;
    worker.kill().await;

    let worker = Worker::start_with(
        state.path(),
        &cwd,
        &config,
        &["-c", "--model", "one/model-a"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut b = Client::initialize(&worker, &token, "b").await;
    b.attach(b.committed).await;
    send_and_wait(&mut b, "after the kill").await;
    let body = server.body().await;
    let checkpoint = first_user_text(&body, 1);
    assert!(
        checkpoint.contains("## Checkpoint 1") && checkpoint.contains("keep the API notes"),
        "the resume starts from the compaction: {checkpoint}"
    );
}

#[tokio::test]
async fn compact_request_survives_a_resume() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let mut server = serve(vec![HTTP_REPLY.to_string(); 6]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    long_session(&mut a).await;
    for _ in 0..4 {
        let _ = server.body().await;
    }
    let asked = a
        .request("compact", r#"{"note":"keep the API notes"}"#)
        .await;
    assert!(a.answer(asked).await["result"].is_object());
    a.until(|frame| is_event(frame, "compacted")).await;
    // A message after the compaction goes to the journal after the base.
    send_and_wait(&mut a, "after the compaction").await;
    let _ = server.body().await;
    let mut worker = worker;
    worker.term().await;

    let worker = Worker::start_with(
        state.path(),
        &cwd,
        &config,
        &["-c", "--model", "one/model-a"],
    )
    .await;
    let token = worker.entry().await.token;
    let mut b = Client::initialize(&worker, &token, "b").await;
    b.attach(b.committed).await;
    send_and_wait(&mut b, "more").await;
    let mut worker = worker;
    worker.kill().await;

    let body = server.body().await;
    assert_eq!(
        first_user_text(&body, 0),
        "task",
        "a resume starts with the first user message: {body}"
    );
    let checkpoint = first_user_text(&body, 1);
    assert!(checkpoint.contains("## Checkpoint 1"), "{checkpoint}");
    assert!(
        body.to_string().contains("after the compaction"),
        "the message sent after the compaction survives the resume: {body}"
    );
}

#[tokio::test]
async fn compact_request_with_one_turn_says_nothing_to_compact() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let mut server = serve(vec![HTTP_REPLY.to_string()]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    send_and_wait(&mut a, "task").await;
    let _ = server.body().await;
    let asked = a
        .request("compact", r#"{"note":"keep the API notes"}"#)
        .await;
    assert!(a.answer(asked).await["result"].is_object());
    let seen = a.until(|frame| is_event(frame, "error")).await;
    let line = seen.last().expect("an error line");
    assert_eq!(
        line["params"]["message"], "nothing to compact",
        "the client hears why nothing happened: {line}"
    );
    let mut worker = worker;
    worker.kill().await;
}

/// The nine PNGs a message over the limit names, written in `cwd`, and the
/// text that names them.
async fn nine_images(cwd: &Path, png: &[u8]) -> String {
    let mut text = String::from("look at");
    for index in 0..9 {
        tokio::fs::write(cwd.join(format!("{index}.png")), png)
            .await
            .expect("an image");
        text.push_str(&format!(" @{index}.png"));
    }
    text
}

#[tokio::test]
async fn a_worker_image_from_a_client_reaches_the_request_and_resumes() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let png = shot_png();
    tokio::fs::write(cwd.join("shot.png"), &png)
        .await
        .expect("the image");
    let mut server = serve(vec![HTTP_REPLY.to_string()]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let sent = a
        .request("send", r#"{"text":"look at @shot.png","mode":"queue"}"#)
        .await;
    assert!(a.answer(sent).await["result"].is_object());
    let body = server.body().await;
    let blocks = message_blocks(&body, 0);
    assert_eq!(blocks[0]["text"], "look at @shot.png", "{blocks:?}");
    assert_eq!(blocks[1]["text"], "[image 64x48]", "{blocks:?}");
    assert_eq!(blocks[2]["type"], "image", "{blocks:?}");
    assert_eq!(
        blocks[2]["source"]["data"],
        STANDARD.encode(&png),
        "the image is the file the message named"
    );
    a.until(|frame| is_event(frame, "run_end")).await;

    // The journal holds the message, so a session that continues from it sends
    // the same image again.
    assert!(a.detach().await["result"].is_object());
    let mut worker = worker;
    tokio::time::timeout(WAIT, worker.child.wait())
        .await
        .expect("the worker stops")
        .expect("the worker ended");
    let mut next = serve(vec![HTTP_REPLY.to_string()]).await;
    let out = Command::new(BIN)
        .args([
            "-c",
            "--provider",
            "anthropic",
            "--base-url",
            &next.base,
            "-p",
            "next",
        ])
        .current_dir(&cwd)
        .env("XDG_CONFIG_HOME", state.path().join("none"))
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", state.path().join("none"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .expect("runs reagent");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = next.body().await;
    let blocks = message_blocks(&body, 0);
    assert_eq!(blocks[1]["text"], "[image 64x48]", "{blocks:?}");
    assert_eq!(blocks[2]["source"]["data"], STANDARD.encode(&png));
}

#[tokio::test]
async fn a_worker_image_in_a_steered_message_reaches_the_next_request() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    ask_folder(&cwd).await;
    let png = shot_png();
    tokio::fs::write(cwd.join("shot.png"), &png)
        .await
        .expect("the image");
    let mut server = serve(vec![bash_reply("echo hi"), HTTP_REPLY.to_string()]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let sent = a
        .request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    assert!(a.answer(sent).await["result"].is_object());
    // The run waits for the approval of the call, so it is still open.
    let asked = a.until(|frame| frame["method"] == "approval_request").await;
    let id = asked.last().expect("an approval")["params"]["id"]
        .as_u64()
        .expect("an approval id");
    let steer = a
        .request("send", r#"{"text":"look at @shot.png","mode":"steer"}"#)
        .await;
    assert!(a.answer(steer).await["result"].is_object());
    a.until(|frame| is_event(frame, "user_message")).await;
    let once = a
        .request("approve", &format!("{{\"id\":{id},\"answer\":\"once\"}}"))
        .await;
    assert!(a.answer(once).await["result"].is_object());
    a.until(|frame| is_event(frame, "run_end")).await;

    let _tool_use = server.body().await;
    let body = server.body().await;
    let messages = body["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("no messages: {body}"))
        .len();
    let blocks = message_blocks(&body, messages - 1);
    // The wire puts the tool result and the user message that follows it in
    // one message, so the text is found where it stands.
    let at = blocks
        .iter()
        .position(|block| block["text"] == "look at @shot.png")
        .unwrap_or_else(|| panic!("no steered text in {blocks:?}"));
    assert_eq!(blocks[at + 1]["text"], "[image 64x48]", "{blocks:?}");
    assert_eq!(blocks[at + 2]["type"], "image", "{blocks:?}");
    assert_eq!(blocks[at + 2]["source"]["data"], STANDARD.encode(&png));
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_worker_image_in_the_task_of_a_worker_reaches_the_first_request() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let png = shot_png();
    tokio::fs::write(cwd.join("shot.png"), &png)
        .await
        .expect("the image");
    let mut server = serve(vec![HTTP_REPLY.to_string()]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let mut worker = Worker::start_with(
        state.path(),
        &cwd,
        &config,
        &["-p", "look at @shot.png", "--model", "one/model-a"],
    )
    .await;

    let body = server.body().await;
    let blocks = message_blocks(&body, 0);
    assert!(
        blocks[0]["text"]
            .as_str()
            .expect("text")
            .starts_with("look at @shot.png"),
        "the task text goes as it was written: {}",
        blocks[0]["text"]
    );
    assert_eq!(blocks[1]["text"], "[image 64x48]", "{blocks:?}");
    assert_eq!(blocks[2]["type"], "image", "{blocks:?}");
    assert_eq!(blocks[2]["source"]["data"], STANDARD.encode(&png));
    worker.kill().await;
}

#[tokio::test]
async fn a_worker_image_over_the_limit_is_an_error_and_the_worker_serves_on() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let png = shot_png();
    let text = nine_images(&cwd, &png).await;
    let mut server = serve(vec![HTTP_REPLY.to_string(), HTTP_REPLY.to_string()]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let params = serde_json::json!({ "text": text, "mode": "queue" }).to_string();
    let sent = a.request("send", &params).await;
    assert!(
        a.answer(sent).await["result"].is_object(),
        "the text is still taken: {text}"
    );
    let seen = a.until(|frame| is_event(frame, "error")).await;
    let message = seen.last().expect("an error line")["params"]["message"]
        .as_str()
        .expect("a message")
        .to_string();
    assert!(message.contains("at most 8 images"), "{message}");
    assert!(message.contains("the image was not attached"), "{message}");
    let body = server.body().await;
    assert!(
        !body.to_string().contains(&STANDARD.encode(&png)),
        "no image reaches the request: {body}"
    );
    let blocks = message_blocks(&body, 0);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(blocks[0]["text"], text, "the text goes as it was written");
    a.until(|frame| is_event(frame, "run_end")).await;

    // The worker still serves the client that read the error.
    send_and_wait(&mut a, "again").await;
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn a_worker_image_word_on_a_text_file_sends_no_image() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    tokio::fs::write(cwd.join("notes.txt"), b"notes\n")
        .await
        .expect("the notes");
    let mut server = serve(vec![HTTP_REPLY.to_string()]).await;
    let config = anthropic_config(state.path(), &server.base).await;
    let worker = Worker::start_with(state.path(), &cwd, &config, &["--model", "one/model-a"]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;

    let sent = a
        .request(
            "send",
            r#"{"text":"mail me@host.com about @notes.txt","mode":"queue"}"#,
        )
        .await;
    assert!(a.answer(sent).await["result"].is_object());
    let frames = a.until(|frame| is_event(frame, "run_end")).await;
    assert!(
        events(frames).iter().all(|event| event["type"] != "error"),
        "the text file raises no error"
    );
    let body = server.body().await;
    let blocks = message_blocks(&body, 0);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(blocks[0]["text"], "mail me@host.com about @notes.txt");
    let mut worker = worker;
    worker.kill().await;
}
