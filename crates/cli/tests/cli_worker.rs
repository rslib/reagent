//! End-to-end tests of `reagent worker`: the ready line, the handshake, the
//! files it leaves behind and the ways it stops.
//!
//! Each test starts the binary against a scripted fake provider and a state
//! folder of its own, so these tests exist only with the `testing` feature.
#![cfg(feature = "testing")]

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use reagent_protocol::discovery;
use reagent_protocol::message::RpcError;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for a line, a file or an exit before it fails.
const WAIT: Duration = Duration::from_secs(30);

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

/// Runs `reagent` to its end with the given arguments.
async fn run(state: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", state)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .expect("the binary runs")
}

/// Waits until the session of a worker is in the list of its folder, which the
/// worker writes after its ready line.
async fn wait_listed(state: &Path, cwd: &Path, session: &str) {
    let cwd = tokio::fs::canonicalize(cwd).await.expect("a real path");
    let folder = reagent_store::paths::sessions_root(state)
        .join(reagent_store::paths::cwd_key(&cwd))
        .join(session);
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if tokio::fs::try_exists(&folder)
            .await
            .expect("a readable folder")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the session {session} is listed");
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

    fn session(&self) -> &str {
        self.ready["session"].as_str().expect("a session id")
    }

    fn socket(&self) -> PathBuf {
        PathBuf::from(self.ready["socket"].as_str().expect("a socket path"))
    }

    fn folder(&self) -> PathBuf {
        self.socket()
            .parent()
            .map(Path::to_path_buf)
            .expect("the socket names its folder")
    }

    fn entry_path(&self) -> PathBuf {
        self.folder().join(format!("{}.json", self.session()))
    }

    async fn entry(&self) -> discovery::Entry {
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

    /// Waits for the exit of the worker, and says how long it took.
    async fn stop(mut self) -> (std::process::ExitStatus, Duration) {
        let started = Instant::now();
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("the worker stops in time")
            .expect("the worker is waited for");
        (status, started.elapsed())
    }

    /// Stops the worker without waiting for its own exit.
    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Sends one line to the worker.
async fn send(stream: &mut UnixStream, line: &str) {
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("writes the line");
}

/// The next answer of the worker, or `None` when it closed the connection.
async fn next_answer(stream: &mut UnixStream) -> Option<Value> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(WAIT, stream.read(&mut byte))
            .await
            .expect("an answer in time")
            .expect("reads the answer");
        if read == 0 {
            assert!(line.is_empty(), "the worker closed after half a line");
            return None;
        }
        if byte[0] == b'\n' {
            return Some(serde_json::from_slice(&line).expect("the answer is JSON"));
        }
        line.push(byte[0]);
    }
}

/// The one line of the handshake, with the major and the token it carries.
fn initialize(id: u64, major: u32, token: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"initialize\",\"params\":\
         {{\"major\":{major},\"client\":{{\"name\":\"test\",\"version\":\"1\"}},\
         \"capabilities\":[],\"token\":\"{token}\"}}}}"
    )
}

#[tokio::test]
async fn worker_ready_line_names_the_session_and_the_socket() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;

    assert_eq!(worker.ready["type"], "ready", "{:?}", worker.ready);
    assert_eq!(
        worker.ready["pid"].as_u64(),
        worker.child.id().map(u64::from),
        "the ready line names the worker"
    );
    let folder = discovery::folder(state.path());
    assert_eq!(
        worker.folder(),
        folder,
        "the socket sits in the entry folder"
    );
    assert!(worker.folder().ends_with("reagent/workers/v1"));
    assert_eq!(
        worker.socket(),
        discovery::socket_path(&folder, worker.session()).expect("a socket path")
    );
    // The entry file is private and sits in a private folder.
    let entry = worker.entry().await;
    assert_eq!(
        entry.session,
        worker.session(),
        "the entry names the session"
    );
    assert_eq!(entry.pid, worker.child.id().expect("a pid"));
    assert_eq!(
        entry.cwd,
        tokio::fs::canonicalize(&cwd).await.expect("a real path"),
        "the entry names the working folder"
    );
    assert_eq!(entry.major, 1);
    assert!(!entry.token.is_empty(), "the entry carries a token");
    use std::os::unix::fs::PermissionsExt;
    let file = tokio::fs::metadata(worker.entry_path())
        .await
        .expect("the entry exists");
    assert_eq!(
        file.permissions().mode() & 0o777,
        0o600,
        "the entry is 0600"
    );
    let dir = tokio::fs::metadata(worker.folder())
        .await
        .expect("the folder exists");
    assert_eq!(
        dir.permissions().mode() & 0o777,
        0o700,
        "the folder is 0700"
    );

    // A client can connect.
    let mut stream = worker.connect().await;
    send(&mut stream, &initialize(1, 1, &entry.token)).await;
    assert!(
        next_answer(&mut stream).await.is_some(),
        "the worker answers"
    );
    drop(stream);
    let mut worker = worker;
    worker.kill().await;
}

#[tokio::test]
async fn worker_initialize_answers_with_client_one_and_the_session() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let token = worker.entry().await.token;

    let mut stream = worker.connect().await;
    send(&mut stream, &initialize(7, 1, &token)).await;
    let answer = next_answer(&mut stream).await.expect("an answer");
    assert_eq!(answer["id"], 7, "the answer names the request");
    let result = &answer["result"];
    assert_eq!(result["major"], 1);
    assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["session"], worker.session());
    assert_eq!(result["client"], 1, "the first client is one");
    assert_eq!(result["capabilities"], serde_json::json!([]));
    assert_eq!(result["committed"], 0, "a new session has no record yet");

    // The client that leaves lets the worker stop.
    send(
        &mut stream,
        "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"detach\"}",
    )
    .await;
    let answer = next_answer(&mut stream).await.expect("the detach answer");
    assert!(answer["result"].is_object(), "{answer}");
    assert!(answer["result"].as_object().expect("an object").is_empty());
    let (status, _) = worker.stop().await;
    assert_eq!(status.code(), Some(0));
}

#[tokio::test]
async fn worker_refuses_a_wrong_token_and_a_wrong_major() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let mut worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let entry = worker.entry().await;

    let mut stream = worker.connect().await;
    send(&mut stream, &initialize(1, 1, "not-the-token")).await;
    let answer = next_answer(&mut stream).await.expect("an answer");
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["error"]["code"], RpcError::WRONG_TOKEN);
    assert!(answer["result"].is_null(), "{answer}");
    assert!(
        answer.to_string().find(&entry.token).is_none(),
        "the answer never carries the token"
    );
    assert!(
        next_answer(&mut stream).await.is_none(),
        "the connection closes"
    );

    let mut stream = worker.connect().await;
    send(&mut stream, &initialize(2, 2, &entry.token)).await;
    let answer = next_answer(&mut stream).await.expect("an answer");
    assert_eq!(answer["id"], 2);
    assert_eq!(answer["error"]["code"], RpcError::WRONG_VERSION);
    assert!(
        next_answer(&mut stream).await.is_none(),
        "the connection closes"
    );
    worker.kill().await;
}

#[tokio::test]
async fn worker_refuses_a_first_line_that_is_not_initialize() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let mut worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;

    let mut stream = worker.connect().await;
    send(
        &mut stream,
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"send\",\"params\":{\"text\":\"hi\"}}",
    )
    .await;
    let answer = next_answer(&mut stream).await.expect("an answer");
    assert_eq!(answer["id"], 3);
    assert_eq!(answer["error"]["code"], RpcError::NOT_INITIALIZED);
    assert!(
        next_answer(&mut stream).await.is_none(),
        "the connection closes"
    );
    worker.kill().await;
}

#[tokio::test]
async fn worker_exits_and_clears_its_files_when_its_only_client_detaches() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let (entry, socket) = (worker.entry_path(), worker.socket());
    let lock = worker.folder().join(format!("{}.lock", worker.session()));
    let token = worker.entry().await.token;
    assert!(
        tokio::fs::try_exists(&entry)
            .await
            .expect("a readable entry")
    );
    assert!(
        tokio::fs::try_exists(&socket)
            .await
            .expect("a readable socket")
    );
    assert!(tokio::fs::try_exists(&lock).await.expect("a readable lock"));

    let mut stream = worker.connect().await;
    send(&mut stream, &initialize(1, 1, &token)).await;
    assert!(
        next_answer(&mut stream).await.is_some(),
        "the worker answers"
    );
    send(
        &mut stream,
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"detach\"}",
    )
    .await;
    assert!(
        next_answer(&mut stream).await.is_some(),
        "the detach answer"
    );

    let (status, elapsed) = worker.stop().await;
    assert_eq!(status.code(), Some(0), "a worker that stops exits 0");
    assert!(
        elapsed < Duration::from_secs(1),
        "the worker stops at once, took {elapsed:?}"
    );
    assert!(
        !tokio::fs::try_exists(&entry)
            .await
            .expect("a readable entry")
    );
    assert!(
        !tokio::fs::try_exists(&socket)
            .await
            .expect("a readable socket")
    );
    // The lock file stays: unlinking it while it is locked would let two
    // later workers lock two different files of one session.
    assert!(tokio::fs::try_exists(&lock).await.expect("a readable lock"));
}

#[tokio::test]
async fn worker_stops_on_sigterm_with_a_client_attached() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let (entry, socket) = (worker.entry_path(), worker.socket());
    let token = worker.entry().await.token;
    let mut stream = worker.connect().await;
    send(&mut stream, &initialize(1, 1, &token)).await;
    assert!(
        next_answer(&mut stream).await.is_some(),
        "the worker answers"
    );

    let pid = worker.child.id().expect("a running worker").to_string();
    let killed = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .await
        .expect("kill runs");
    assert!(killed.success());

    let (status, _) = worker.stop().await;
    assert!(
        status.code().is_some(),
        "the worker exits by itself: {status:?}"
    );
    assert!(
        !tokio::fs::try_exists(&entry)
            .await
            .expect("a readable entry")
    );
    assert!(
        !tokio::fs::try_exists(&socket)
            .await
            .expect("a readable socket")
    );
    drop(stream);
}

#[tokio::test]
async fn worker_with_no_task_waits_ten_seconds_for_a_client() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (status, elapsed) = worker.stop().await;
    assert_eq!(status.code(), Some(0), "a worker that stops exits 0");
    assert!(
        elapsed >= Duration::from_secs(5),
        "the worker waits for a client, took {elapsed:?}"
    );
}

#[tokio::test]
async fn worker_refuses_a_second_worker_of_the_same_session() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;
    let mut worker = Worker::start(state.path(), &cwd, &["--provider", &provider]).await;
    let pid = worker.child.id().expect("a pid");
    let session = worker.session().to_string();
    wait_listed(state.path(), &cwd, &session).await;
    // A joined client keeps the first worker up: with none, it stops after its
    // first-client wait, which a slow start of the second one can outlast.
    let token = worker.entry().await.token;
    let mut stream = worker.connect().await;
    send(&mut stream, &initialize(1, 1, &token)).await;
    assert!(
        next_answer(&mut stream).await.is_some(),
        "the worker answers"
    );

    let second = run(
        state.path(),
        &cwd,
        &["worker", "-r", &session, "--provider", &provider],
    )
    .await;
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains(&format!("session {session} has a worker (pid {pid})")),
        "the error names the worker: {stderr}"
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).is_empty(),
        "a refused worker prints no ready line"
    );

    // The first worker still serves its session.
    let mut again = worker.connect().await;
    send(&mut again, &initialize(1, 1, &token)).await;
    let answer = next_answer(&mut again).await.expect("an answer");
    assert_eq!(answer["result"]["session"], session);
    drop(stream);
    worker.kill().await;
}

#[tokio::test]
async fn worker_starts_on_a_finished_session_with_no_message() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": [{"text": "hi"}]}"#).await;
    let first = run(
        state.path(),
        &cwd,
        &["-p", "hi", "--json", "--provider", &provider],
    )
    .await;
    assert!(first.status.success(), "{first:?}");
    let start: Value = serde_json::from_slice(
        first
            .stdout
            .split(|byte| *byte == b'\n')
            .next()
            .expect("a line"),
    )
    .expect("the start line is JSON");
    let session = start["session"].as_str().expect("a session id").to_string();

    let mut worker = Worker::start(
        state.path(),
        &cwd,
        &["-r", &session, "--provider", &provider],
    )
    .await;
    assert_eq!(worker.session(), session, "the worker serves that session");
    worker.kill().await;
}

#[tokio::test]
async fn worker_refuses_json_and_no_session() {
    let state = state();
    let cwd = state.path().join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    let provider = script(state.path(), r#"{"replies": []}"#).await;

    let json = run(
        state.path(),
        &cwd,
        &["worker", "-p", "hi", "--json", "--provider", &provider],
    )
    .await;
    assert_eq!(json.status.code(), Some(2), "{json:?}");
    let stderr = String::from_utf8_lossy(&json.stderr);
    assert!(stderr.contains("--json"), "{stderr}");
    assert!(stderr.contains("worker"), "{stderr}");

    let none = run(
        state.path(),
        &cwd,
        &[
            "worker",
            "-p",
            "hi",
            "--no-session",
            "--provider",
            &provider,
        ],
    )
    .await;
    assert_eq!(none.status.code(), Some(2), "{none:?}");
    let stderr = String::from_utf8_lossy(&none.stderr);
    assert!(stderr.contains("--no-session"), "{stderr}");
}
