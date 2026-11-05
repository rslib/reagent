//! Crash tests: a run killed in a turn, and a part damaged after a crash.
//!
//! Each test runs the binary against a temp home, so no test touches the real
//! sessions. The model is a scripted provider for the killed run, and a local
//! chat server for the run that continues it, which records the request it
//! gets. The scripted provider exists only with `testing` on.

#![cfg(feature = "testing")]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
/// How long a test waits for a file a run makes.
const WAIT_MS: u64 = 5000;
/// The environment variable the mock server takes its key from.
const KEY_ENV: &str = "CRASH_KEY";
/// A chat reply that ends the turn with text.
const CHAT_REPLY: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

/// A temp home the runs write their state in, and a temp working folder.
struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            root: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// The folder of the state tree a run writes its sessions in.
    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// The working folder a run sees.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes the folder it runs in"
    )]
    fn cwd(&self) -> PathBuf {
        let cwd = self.root.path().join("work");
        std::fs::create_dir_all(&cwd).expect("create the working folder");
        cwd
    }

    /// Writes a fake provider script and returns its path.
    async fn script(&self, name: &str, replies: Value) -> PathBuf {
        let path = self.root.path().join(name);
        tokio::fs::write(&path, json!({ "replies": replies }).to_string())
            .await
            .expect("write the script");
        path
    }

    /// A run of the binary in the working folder, with its output in pipes.
    fn spawn(&self, args: &[&str]) -> Child {
        self.command(args)
            .stdin(Stdio::null())
            .spawn()
            .expect("start the binary")
    }

    /// A command for one run of the binary.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(self.root.path().join("work"))
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.state())
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            .env(KEY_ENV, "sk-test")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs the binary and waits for it.
    async fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().await.expect("run the binary")
    }

    /// The folder that holds the sessions of a working folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test names the session folder of its own temp folder"
    )]
    fn folder(&self, cwd: &Path) -> PathBuf {
        let cwd = std::fs::canonicalize(cwd).expect("canonical working folder");
        self.state()
            .join("reagent/sessions")
            .join(reagent_store::paths::cwd_key(&cwd))
    }

    /// The ids of the sessions of a working folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test lists the session folder it made"
    )]
    fn ids(&self, cwd: &Path) -> Vec<String> {
        let mut ids = std::fs::read_dir(self.folder(cwd))
            .map(|dir| {
                dir.filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .filter_map(|path| Some(path.file_name()?.to_str()?.to_owned()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// The only session of a working folder.
    fn only_id(&self, cwd: &Path) -> String {
        let ids = self.ids(cwd);
        assert_eq!(ids.len(), 1, "one session: {ids:?}");
        ids[0].clone()
    }

    /// The folder of the only session of a working folder.
    fn only_session(&self, cwd: &Path) -> PathBuf {
        self.folder(cwd).join(self.only_id(cwd))
    }
}

/// The stdout of a run, as text.
fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a run, as text.
fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The lines a file holds, 0 when it is missing.
#[expect(
    clippy::disallowed_methods,
    reason = "a test reads the marker file a run writes"
)]
fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// Waits until `path` exists and holds a byte. A shell makes the file of a
/// `>>` before the command writes to it, so the file alone is not proof the
/// command ran.
async fn wait_for(path: &Path, limit: Duration) {
    let started = std::time::Instant::now();
    while tokio::fs::metadata(path)
        .await
        .map_or(true, |meta| meta.len() == 0)
    {
        assert!(
            started.elapsed() < limit,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sends SIGKILL to a run and reaps it.
async fn kill(child: &mut Child) {
    child.kill().await.expect("kill the run");
    child.wait().await.expect("reap the run");
}

/// The JSON lines `session cat` prints for a session.
async fn cat(home: &Home, id: &str) -> String {
    let output = home.run(&["session", "cat", id]).await;
    assert!(output.status.success(), "{}", err(&output));
    out(&output)
}

/// The newest part file of a session folder.
async fn newest_part(dir: &Path) -> PathBuf {
    let mut ids = reagent_store::paths::part_ids(dir).expect("the part files");
    ids.sort_unstable();
    let id = ids.pop().expect("a part file");
    reagent_store::paths::part_path(dir, id)
}

/// The bytes the footer and trailer of a closed part take at its end.
fn footer_len(bytes: &[u8]) -> usize {
    let trailer = &bytes[bytes.len() - reagent_store::part::TRAILER_LEN..];
    let frame = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) as usize;
    reagent_store::part::TRAILER_LEN + frame
}

/// A chat server that records the body of every request it answers.
struct Mock {
    /// The base URL a run points at.
    base: String,
    /// The bodies it got, oldest first.
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    /// Binds a port and serves chat replies in the background.
    async fn start() -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port");
        let address = listener.local_addr().expect("the bound address");
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bodies);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let Some(body) = read_request(&mut socket).await else {
                        return;
                    };
                    sink.lock().expect("the recorded bodies").push(body);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{CHAT_REPLY}",
                        CHAT_REPLY.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Mock {
            base: format!("http://{address}/v1"),
            bodies,
        }
    }

    /// The bodies it got so far.
    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("the recorded bodies").clone()
    }

    /// The arguments that point a run at this server.
    fn args(&self) -> Vec<String> {
        [
            "--provider",
            "openai-chat",
            "--model",
            "resume-model",
            "--api-key-env",
            KEY_ENV,
            "--base-url",
            &self.base,
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

/// Reads one HTTP request: its head, then the body its length names.
async fn read_request(socket: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let split = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = buffer[split..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// A bash call killed mid-flight is reported, never run again.
#[tokio::test]
async fn a_killed_bash_call_is_not_run_again() {
    let home = Home::new();
    let cwd = home.cwd();
    let marker = cwd.join("ran.txt");
    let script = home
        .script(
            "crash.json",
            json!([{ "calls": [{ "name": "bash", "arguments": {
                "command": format!("echo ran >> {}; sleep 5", marker.display()),
            } }] }]),
        )
        .await;
    let provider = format!("fake:{}", script.display());
    let mut child = home.spawn(&["-p", "touch the marker", "--provider", &provider]);
    // The marker means the command ran, and its start was synced before that.
    wait_for(&marker, Duration::from_millis(WAIT_MS)).await;
    kill(&mut child).await;
    assert_eq!(lines(&marker), 1, "the command did not run");

    // The session holds the call, with no result.
    let id = home.only_id(&cwd);
    let log = cat(&home, &id).await;
    assert!(log.contains("\"call_start\""), "{log}");
    assert!(!log.contains("\"tool_result\""), "{log}");

    // A continue with no message asks the model again, and the command is not
    // run a second time.
    let mock = Mock::start().await;
    let args = ["-c".to_owned()]
        .into_iter()
        .chain(mock.args())
        .collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = home.run(&args).await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(lines(&marker), 1, "the bash command ran again");

    // The next request holds the interrupted result, after the call it answers.
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 1, "one request after the resume");
    let body = &bodies[0];
    let interrupted = "interrupted: check whether it ran";
    assert!(body.contains(interrupted), "{body}");
    assert!(body.contains("call_0"), "{body}");
    let log = cat(&home, &id).await;
    let call = log.find("\"call_start\"").expect("the call");
    let result = log.find(interrupted).expect("the result");
    assert!(call < result, "the result comes after the call: {log}");
    assert_eq!(
        log.matches("\"tool_result\"").count(),
        1,
        "one result for the call: {log}"
    );
}

/// A read a crash left without a result runs again, and sees the new world.
#[tokio::test]
async fn a_killed_read_runs_again() {
    let home = Home::new();
    let cwd = home.cwd();
    let marker = cwd.join("ran.txt");
    let data = cwd.join("data.txt");
    tokio::fs::write(&data, "before the crash\n")
        .await
        .expect("write the file");
    // The bash call is synced before it runs, which also puts the assistant
    // message on the disk. The read after it never starts: the run is killed
    // while the bash command sleeps, and a read is never reported as
    // interrupted, so the continue runs it again.
    let script = home
        .script(
            "crash.json",
            json!([{ "calls": [
                { "name": "bash", "arguments": {
                    "command": format!("echo ran >> {}; sleep 5", marker.display()),
                } },
                { "name": "read", "arguments": { "path": "data.txt" } }
            ] }]),
        )
        .await;
    let provider = format!("fake:{}", script.display());
    let mut child = home.spawn(&["-p", "read the file", "--provider", &provider]);
    wait_for(&marker, Duration::from_millis(WAIT_MS)).await;
    kill(&mut child).await;

    // The session holds both calls and no result.
    let id = home.only_id(&cwd);
    let log = cat(&home, &id).await;
    assert!(log.contains("\"read\""), "{log}");
    assert!(!log.contains("\"tool_result\""), "{log}");

    // The continue runs the read again, and it sees what the crash left.
    tokio::fs::write(&data, "after the crash\n")
        .await
        .expect("write the file");
    let mock = Mock::start().await;
    let args = ["-c".to_owned()]
        .into_iter()
        .chain(mock.args())
        .collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = home.run(&args).await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(lines(&marker), 1, "the bash command ran again");
    let log = cat(&home, &id).await;
    assert!(log.contains("after the crash"), "{log}");
    // The model got the result of the read it never saw.
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 1, "one request after the resume");
    assert!(bodies[0].contains("after the crash"), "{}", bodies[0]);
    assert!(
        bodies[0].contains("interrupted: check whether it ran"),
        "{}",
        bodies[0]
    );
}

/// A run killed in its first request leaves a message with no reply, and a
/// continue with no message asks the model for that reply.
#[tokio::test]
async fn a_continue_answers_a_message_left_with_no_reply() {
    let home = Home::new();
    let cwd = home.cwd();
    // A server that takes the request and never answers it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port");
    let base = format!("http://{}/v1", listener.local_addr().expect("the address"));
    let mut child = home.spawn(&[
        "-p",
        "hello",
        "--provider",
        "openai-chat",
        "--model",
        "resume-model",
        "--api-key-env",
        KEY_ENV,
        "--base-url",
        &base,
    ]);
    let (_socket, _) = tokio::time::timeout(Duration::from_millis(WAIT_MS), listener.accept())
        .await
        .expect("the run sends its request")
        .expect("accept the request");
    // The writer thread puts the message on the disk before the request goes
    // out; give it a moment to finish the write.
    tokio::time::sleep(Duration::from_millis(200)).await;
    kill(&mut child).await;
    let id = home.only_id(&cwd);
    assert!(
        cat(&home, &id).await.contains("hello"),
        "the message is saved"
    );

    let mock = Mock::start().await;
    let args = ["-c".to_owned()]
        .into_iter()
        .chain(mock.args())
        .collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = home.run(&args).await;
    assert!(output.status.success(), "{}", err(&output));
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 1, "the continue asks the model");
    assert!(bodies[0].contains("hello"), "{}", bodies[0]);
    let log = cat(&home, &id).await;
    assert_eq!(log.matches("\"turn_end\"").count(), 1, "{log}");
}

/// A part cut in its last frame resumes from the last complete commit.
#[tokio::test]
async fn a_part_cut_in_its_last_frame_resumes() {
    let home = Home::new();
    let cwd = home.cwd();
    let first = home
        .script(
            "first.json",
            json!([
                { "calls": [{ "name": "bash", "arguments": { "command": "echo one" } }] },
                { "text": "first turn" }
            ]),
        )
        .await;
    assert!(
        home.run(&[
            "-p",
            "one",
            "--provider",
            &format!("fake:{}", first.display())
        ])
        .await
        .status
        .success()
    );
    let part = newest_part(&home.only_session(&cwd)).await;
    // A crash leaves no footer: drop it, then cut inside the last frame. The
    // tail is torn, the commits before it are whole.
    let bytes = tokio::fs::read(&part).await.expect("read the part");
    let len = (bytes.len() - footer_len(&bytes)) as u64;
    let cut = (reagent_store::frame::OVERHEAD + 2) as u64;
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .await
        .expect("open the part");
    file.set_len(len - cut).await.expect("cut the tail");

    let second = home
        .script("second.json", json!([{ "text": "more" }]))
        .await;
    let output = home
        .run(&[
            "-c",
            "-p",
            "more",
            "--provider",
            &format!("fake:{}", second.display()),
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    // The records up to the last complete commit stand, the resume forked from
    // them, and the message it carried was written after them.
    let log = cat(&home, &home.only_id(&cwd)).await;
    assert!(
        !log.contains("first turn"),
        "the torn commit is gone: {log}"
    );
    // The turn the resume made follows the last one the chain kept.
    assert!(log.contains("\"turn\":2"), "the resumed turn: {log}");
    assert!(log.contains("\"fork\":{"), "the resume forked: {log}");
    assert!(log.contains("\"text\":\"more"), "the new message: {log}");
}

/// A part with one flipped byte resumes from the last complete commit.
#[tokio::test]
async fn a_part_with_one_flipped_byte_resumes() {
    let home = Home::new();
    let cwd = home.cwd();
    let first = home
        .script(
            "first.json",
            json!([
                { "calls": [{ "name": "bash", "arguments": { "command": "echo one" } }] },
                { "text": "first turn" }
            ]),
        )
        .await;
    assert!(
        home.run(&[
            "-p",
            "one",
            "--provider",
            &format!("fake:{}", first.display())
        ])
        .await
        .status
        .success()
    );
    let part = newest_part(&home.only_session(&cwd)).await;
    // One damaged byte near the end of the last frame before the footer: the
    // frame it is in cannot be trusted, and the commits before it stand.
    let mut bytes = tokio::fs::read(&part).await.expect("read the part");
    let at = bytes.len() - footer_len(&bytes) - 3;
    bytes[at] ^= 0xff;
    tokio::fs::write(&part, &bytes)
        .await
        .expect("damage the part");

    let second = home
        .script("second.json", json!([{ "text": "more" }]))
        .await;
    let output = home
        .run(&[
            "-c",
            "-p",
            "more",
            "--provider",
            &format!("fake:{}", second.display()),
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    // The records up to the last complete commit stand, and the resume forked
    // from them.
    let log = cat(&home, &home.only_id(&cwd)).await;
    assert!(
        !log.contains("first turn"),
        "the damaged commit is gone: {log}"
    );
    // The turn the resume made follows the last one the chain kept.
    assert!(log.contains("\"turn\":2"), "the resumed turn: {log}");
    assert!(log.contains("\"fork\":{"), "the resume forked: {log}");
    assert!(log.contains("\"text\":\"more"), "the new message: {log}");
}

/// A continue with a message answers the calls of the last reply first, and
/// the message comes after their results.
#[tokio::test]
async fn a_continue_with_a_message_answers_the_calls_first() {
    let home = Home::new();
    let cwd = home.cwd();
    let marker = cwd.join("ran.txt");
    let data = cwd.join("data.txt");
    tokio::fs::write(&data, "before the crash\n")
        .await
        .expect("write the file");
    let script = home
        .script(
            "crash.json",
            json!([{ "calls": [
                { "name": "bash", "arguments": {
                    "command": format!("echo ran >> {}; sleep 5", marker.display()),
                } },
                { "name": "read", "arguments": { "path": "data.txt" } }
            ] }]),
        )
        .await;
    let provider = format!("fake:{}", script.display());
    let mut child = home.spawn(&["-p", "read the file", "--provider", &provider]);
    wait_for(&marker, Duration::from_millis(WAIT_MS)).await;
    kill(&mut child).await;

    // The continue carries a message, which the run sends after it answered the
    // calls the crash left open.
    let mock = Mock::start().await;
    let args = ["-c".to_owned(), "-p".to_owned(), "next".to_owned()]
        .into_iter()
        .chain(mock.args())
        .collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = home.run(&args).await;
    assert!(output.status.success(), "{}", err(&output));

    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 1, "one request after the resume");
    let body = &bodies[0];
    // The wire names a tool result by the role of its message.
    let assistant = body.find("\"role\":\"assistant\"").expect("the reply");
    let first = body.find("\"role\":\"tool\"").expect("a result");
    let last = body.rfind("\"role\":\"tool\"").unwrap_or_default();
    let next = body
        .find("\"next\\n")
        .unwrap_or_else(|| panic!("the message: {body}"));
    assert!(assistant < first, "a result answers the reply: {body}");
    assert!(last < next, "the results come before the message: {body}");
    // The log holds the two results of the reply, then the message once.
    let log = cat(&home, &home.only_id(&cwd)).await;
    assert_eq!(
        log.matches("\"tool_result\"").count(),
        2,
        "one result per call: {log}"
    );
    assert_eq!(
        log.matches("\"next\\n").count(),
        1,
        "the message once: {log}"
    );
    assert!(log.contains("interrupted: check whether it ran"), "{log}");
}
