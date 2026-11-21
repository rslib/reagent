//! End-to-end tests of `reagent gateway`: the parked questions it hands to the
//! gateway plugins of its config folder, and the answers it takes back.
//!
//! Each test writes its gateway plugin into the config folder of a temp home, so
//! these tests exist only with the `testing` feature on.
#![cfg(feature = "testing")]

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use crate::worker_live::{Client, Worker, ask_folder, ask_script, config_of, script, state};

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
const WAIT: Duration = Duration::from_secs(60);
const MARK: Duration = Duration::from_millis(50);

/// One test's own state, config and working folder.
struct Home {
    state: PathBuf,
    config: PathBuf,
    work: PathBuf,
    events: PathBuf,
    answers: PathBuf,
    _dir: tempfile::TempDir,
}

impl Home {
    /// A home whose config holds `body` and one gateway plugin that appends each
    /// event to a file and answers from another.
    async fn new(body: &str) -> Home {
        let dir = state();
        let state = dir.path().to_path_buf();
        let work = state.join("work");
        let events = state.join("events.jsonl");
        let answers = state.join("answers.json");
        tokio::fs::create_dir(&work).await.expect("the work folder");
        ask_folder(&work).await;
        let provider = script(&state, &ask_script()).await;
        let config = config_of(
            &state,
            &format!("{body}\n[providers.alpha]\nwire = \"{provider}\"\n"),
        )
        .await;
        let home = Home {
            state,
            config,
            work,
            events,
            answers,
            _dir: dir,
        };
        home.plugin("telegram", &home.gateway_plugin()).await;
        home
    }

    /// The entry module of the test's gateway plugin, which logs every event it
    /// hears and answers the question the answers file names.
    fn gateway_plugin(&self) -> String {
        gateway_plugin_at(&self.events, &self.answers)
    }
}

/// The entry module of a gateway plugin that logs every event it hears to
/// `events` and asks the gateway what the file `answers` holds.
fn gateway_plugin_at(events: &std::path::Path, answers: &std::path::Path) -> String {
    let events = events.display();
    let answers = answers.display();
    format!(
        r#"
import {{ readFileSync, writeFileSync }} from "node:fs";

const events = {events:?};
const answers = {answers:?};

const lines = [];

function log(entry) {{
  lines.push(JSON.stringify(entry));
  writeFileSync(events, lines.join("\n") + "\n");
}}

export default function (reagent) {{
  reagent.on("question_parked", (event) => {{
    log({{ event: "question_parked", ...event }});
  }});
  reagent.on("question_closed", (event) => {{
    log({{ event: "question_closed", ...event }});
  }});
  reagent.on("approval_parked", (event) => {{
    log({{ event: "approval_parked", ...event }});
  }});
  reagent.on("approval_closed", (event) => {{
    log({{ event: "approval_closed", ...event }});
  }});
  reagent.on("question_forwarded", (event) => {{
    log({{ event: "question_forwarded", ...event }});
  }});
  // The test writes one request at a time; a changed file is a new answer.
  let last = "";
  setInterval(() => {{
    let text;
    try {{
      text = readFileSync(answers, "utf8");
    }} catch {{
      return;
    }}
    if (text === last) {{
      return;
    }}
    last = text;
    const ask = JSON.parse(text);
    const verdict =
      ask.forward !== undefined
        ? reagent.gateway.forward(ask.qid, ask.token, ask.forward, ask.note)
        : ask.approve === undefined
          ? reagent.gateway.answer(ask.qid, ask.token, ask.answers, ask.by)
          : reagent.gateway.approve(ask.qid, ask.token, ask.approve, ask.by, ask.reason);
    verdict
      .then((verdict) => log({{ event: "answer", verdict }}))
      .catch((error) => log({{ event: "answer", error: String(error) }}));
  }}, 30);
}}
"#
    )
}

impl Home {
    /// Writes one plugin folder of the config, with `role` in its manifest.
    async fn plugin_with(&self, folder: &str, role: &str, entry: &str) {
        let dir = self.plugins().join(folder);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the plugin folder");
        tokio::fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"name": "{folder}", "reagent": {{"entry": "index.js", "role": {role:?}, "permissions": {{"fs": "write"}}}}}}"#
            ),
        )
        .await
        .expect("the manifest");
        tokio::fs::write(dir.join("index.js"), entry)
            .await
            .expect("the entry");
    }

    /// Writes the gateway plugin under the folder `folder`.
    async fn plugin(&self, folder: &str, entry: &str) {
        self.plugin_with(folder, "gateway", entry).await;
    }

    /// Writes the contacts file of the config.
    async fn contacts(&self, body: &str) {
        let dir = self.config.join("reagent");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the config folder");
        tokio::fs::write(dir.join("contacts.toml"), body)
            .await
            .expect("the contacts file");
    }

    /// Adds the gateway plugin `folder`, which logs to its own events file and
    /// reads its own answers file, and returns the two.
    async fn second(&self, folder: &str) -> (PathBuf, PathBuf) {
        let events = self.state.join(format!("{folder}-events.jsonl"));
        let answers = self.state.join(format!("{folder}-answers.json"));
        self.plugin(folder, &gateway_plugin_at(&events, &answers))
            .await;
        (events, answers)
    }

    /// The plugins folder of the config.
    fn plugins(&self) -> PathBuf {
        self.config.join("reagent").join("plugins")
    }

    /// Starts the gateway on this home.
    fn gateway(&self) -> Gateway {
        let child = Command::new(BIN)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_CACHE_HOME", "/nonexistent/reagent-test-cache")
            .env("XDG_STATE_HOME", &self.state)
            .arg("gateway")
            .current_dir(&self.work)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the gateway starts");
        Gateway { child: Some(child) }
    }

    /// Every event the plugin logged once `done` says one is enough.
    async fn told_until(&self, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        told_until_in(&self.events, done).await
    }

    /// Writes the answer the plugin asks the gateway to record.
    async fn answer(&self, request: Value) {
        answer_in(&self.answers, request).await;
    }

    /// The parked entries of the discovery folder of this home.
    async fn parked(&self) -> Vec<reagent_protocol::discovery::Parked> {
        let folder = reagent_protocol::discovery::folder(&self.state);
        reagent_protocol::discovery::list_parked(&folder, |_, _| true).await
    }

    /// Waits for the parked question of the run.
    async fn parked_until(&self) -> reagent_protocol::discovery::Parked {
        for _ in 0..(WAIT.as_millis() / MARK.as_millis()) {
            if let Some(entry) = self.parked().await.pop() {
                return entry;
            }
            tokio::time::sleep(MARK).await;
        }
        panic!("the worker leaves no parked question");
    }

    /// Runs the binary here with `args` and `stdin`, and returns its code, its
    /// output and its errors.
    async fn run(&self, args: &[&str], stdin: &str) -> (i32, String, String) {
        let mut child = Command::new(BIN)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_CACHE_HOME", "/nonexistent/reagent-test-cache")
            .env("XDG_STATE_HOME", &self.state)
            .args(args)
            .current_dir(&self.work)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the command starts");
        let mut input = child.stdin.take().expect("piped stdin");
        input
            .write_all(stdin.as_bytes())
            .await
            .expect("writes the input");
        drop(input);
        let out = tokio::time::timeout(WAIT, child.wait_with_output())
            .await
            .expect("the command ends in time")
            .expect("the command runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// The records of the session `session`, as `session cat` prints them.
    async fn records(&self, session: &str) -> String {
        let (code, out, error) = self.run(&["session", "cat", session], "").await;
        assert_eq!(code, 0, "session cat: {error}");
        out
    }

    /// The records of `session` once they hold `text`.
    async fn records_until(&self, session: &str, text: &str) -> String {
        let mut last = String::new();
        for _ in 0..(WAIT.as_millis() / MARK.as_millis()) {
            last = self.records(session).await;
            if last.contains(text) {
                return last;
            }
            tokio::time::sleep(MARK).await;
        }
        panic!("the records never held {text}: {last}");
    }
}

/// Every event the plugin that logs to `events` logged, one JSON object per
/// line.
async fn told_in(events: &std::path::Path) -> Vec<Value> {
    let text = match tokio::fs::read_to_string(events).await {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Every event the plugin that logs to `events` logged once `done` says one is
/// enough.
async fn told_until_in(events: &std::path::Path, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let mut last = Vec::new();
    for _ in 0..(WAIT.as_millis() / MARK.as_millis()) {
        last = told_in(events).await;
        if done(&last) {
            return last;
        }
        tokio::time::sleep(MARK).await;
    }
    panic!("the plugin never heard it: {last:?}");
}

/// Writes the request the plugin that reads `answers` hands the gateway.
async fn answer_in(answers: &std::path::Path, request: Value) {
    tokio::fs::write(answers, request.to_string())
        .await
        .expect("writes the answer");
}

/// A gateway the test started, with the file its plugin logs to.
struct Gateway {
    child: Option<Child>,
}

impl Gateway {
    /// Stops the gateway with SIGTERM and answers its exit code and errors.
    async fn term(&mut self) -> (i32, String) {
        let child = self.child.take().expect("a running gateway");
        let pid = child.id().expect("a running gateway").to_string();
        let sent = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .await
            .expect("kill runs");
        assert!(sent.success(), "kill -TERM found the gateway");
        let out = tokio::time::timeout(WAIT, child.wait_with_output())
            .await
            .expect("the gateway ends in time")
            .expect("the gateway runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// A run of the home's provider that asks a question, with `a` attached, then
/// parks when `a` leaves: the run's session and its parked entry.
async fn parked(home: &Home) -> (String, reagent_protocol::discovery::Parked) {
    let worker = Worker::start_with(
        &home.state,
        &home.work,
        &home.config,
        &["--model", "alpha/m"],
    )
    .await;
    let session = worker.session().to_string();
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    a.until(|frame| frame["method"] == "question_request").await;
    drop(a);
    let entry = home.parked_until().await;
    let mut worker = worker;
    worker.kill().await;
    (session, entry)
}

/// The provider the restarted run reads: the same wire file, now one reply that
/// ends the turn, so the run cannot ask again.
async fn settle(home: &Home) {
    script(&home.state, r#"{"replies":[{"text":"done"}]}"#).await;
}

/// The first event of the log that is named `name`.
fn event<'a>(told: &'a [Value], name: &str) -> &'a Value {
    told.iter()
        .find(|entry| entry["event"] == name)
        .unwrap_or_else(|| panic!("no {name} in {told:?}"))
}

/// A parked question reaches the plugin with every key, and the answer it takes
/// back continues the run.
#[tokio::test]
async fn gateway_a_parked_question_is_answered_by_a_plugin() {
    let home = Home::new("").await;
    let (session, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();

    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_parked"))
        .await;
    let parked = event(&told, "question_parked").clone();
    assert_eq!(parked["qid"], entry.qid.as_str());
    assert_eq!(parked["session"], session.as_str());
    assert_eq!(parked["questions"][0]["q"], "Which database?");
    assert!(parked["at"].as_u64().is_some(), "{parked}");
    assert!(
        parked["deadline"].as_u64().is_some(),
        "the default deadline is set: {parked}"
    );
    let token = parked["token"].as_str().expect("a token").to_string();
    assert_eq!(token.len(), 32, "{token}");
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{token}"
    );

    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": token,
        "answers": [["sqlite"]],
        "by": "ray",
    }))
    .await;
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "answer"))
        .await;
    assert_eq!(event(&told, "answer")["verdict"], "ok", "{told:?}");
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_closed"))
        .await;
    let closed = event(&told, "question_closed").clone();
    assert_eq!(closed["qid"], entry.qid.as_str());
    assert_eq!(closed["how"], "answered");
    assert_eq!(closed["by"], "gateway:telegram:ray");

    // The run goes on by itself and reads the answer.
    let records = home.records_until(&session, "done").await;
    assert!(
        records.contains("A: sqlite"),
        "the model reads the answer: {records}"
    );
    assert!(home.parked().await.is_empty(), "the entry is gone");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A wrong token is refused and leaves the question open; the answer that works
/// closes it, and the one after that is stale.
#[tokio::test]
async fn gateway_a_wrong_token_is_denied_and_a_second_answer_is_stale() {
    let home = Home::new("").await;
    let (_, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_parked"))
        .await;
    let token = event(&told, "question_parked")["token"]
        .as_str()
        .expect("a token")
        .to_string();

    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": "0".repeat(32),
        "answers": [["sqlite"]],
        "by": "ray",
    }))
    .await;
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "answer"))
        .await;
    assert_eq!(event(&told, "answer")["verdict"], "denied", "{told:?}");
    assert_eq!(
        home.parked().await.len(),
        1,
        "the question the answer was refused for stays open"
    );

    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": token,
        "answers": [["sqlite"]],
        "by": "ray",
    }))
    .await;
    let told = home
        .told_until(|told| {
            told.iter()
                .filter(|entry| entry["event"] == "answer")
                .count()
                == 2
        })
        .await;
    let verdicts: Vec<&Value> = told
        .iter()
        .filter(|entry| entry["event"] == "answer")
        .map(|entry| &entry["verdict"])
        .collect();
    assert_eq!(verdicts[1], "ok", "{told:?}");

    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": token,
        "answers": [["sqlite"]],
        "by": "ray",
        "again": true,
    }))
    .await;
    let told = home
        .told_until(|told| {
            told.iter()
                .filter(|entry| entry["event"] == "answer")
                .count()
                == 3
        })
        .await;
    let verdicts: Vec<&Value> = told
        .iter()
        .filter(|entry| entry["event"] == "answer")
        .map(|entry| &entry["verdict"])
        .collect();
    assert_eq!(verdicts[2], "stale", "{told:?}");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// An answer `reagent answer` writes while the gateway runs closes the question
/// the gateway announced.
#[tokio::test]
async fn gateway_an_answer_of_reagent_answer_closes_the_question() {
    let home = Home::new("").await;
    let (_, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();
    home.told_until(|told| told.iter().any(|entry| entry["event"] == "question_parked"))
        .await;

    let (code, _, error) = home
        .run(&["answer", &entry.qid, "--no-resume"], r#"[["yes"]]"#)
        .await;
    assert_eq!(code, 0, "{error}");
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_closed"))
        .await;
    let closed = event(&told, "question_closed").clone();
    assert_eq!(closed["qid"], entry.qid.as_str());
    assert_eq!(closed["how"], "answered");
    assert_eq!(closed["by"], "cli");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A question nobody answers expires at its deadline: the run goes on with the
/// text a client shows when nobody answered.
#[tokio::test]
async fn gateway_a_deadline_expires_the_question() {
    let home = Home::new("[gateway]\ndeadline = \"2s\"\n").await;
    let (session, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();

    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_closed"))
        .await;
    let closed = event(&told, "question_closed").clone();
    assert_eq!(closed["qid"], entry.qid.as_str());
    assert_eq!(closed["how"], "expired");
    assert_eq!(closed["by"], "deadline");

    let records = home.records_until(&session, "done").await;
    assert!(
        records.contains("No answer; decide yourself and state the choice."),
        "the run goes on after the deadline: {records}"
    );
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A second gateway of the same state folder refuses to start, and SIGTERM ends
/// the first one cleanly.
#[tokio::test]
async fn gateway_a_second_gateway_does_not_start() {
    let home = Home::new("").await;
    parked(&home).await;
    let mut gateway = home.gateway();
    home.told_until(|told| told.iter().any(|entry| entry["event"] == "question_parked"))
        .await;

    let (code, _, error) = home.run(&["gateway"], "").await;
    assert_eq!(code, 1, "{error}");
    assert!(error.contains("a gateway already runs"), "{error}");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A gateway plugin stays out of a normal run: its `run_start` listener never
/// fires, while the same plugin with no role, in the same run, hears it.
#[tokio::test]
async fn gateway_a_plugin_does_not_load_in_a_normal_run() {
    let home = Home::new("").await;
    let heard = |file: &std::path::Path| {
        format!(
            r#"
import {{ writeFileSync }} from "node:fs";

export default function (reagent) {{
  reagent.on("run_start", () => {{
    writeFileSync({file:?}, "{{\"event\":\"run_start\"}}\n");
  }});
}}
"#,
            file = file.display()
        )
    };
    home.plugin("telegram", &heard(&home.events)).await;
    let control = home.state.join("control.jsonl");
    let dir = home.plugins().join("normal");
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("the plugin folder");
    tokio::fs::write(
        dir.join("package.json"),
        r#"{"name": "normal", "reagent": {"entry": "index.js", "permissions": {"fs": "write"}}}"#,
    )
    .await
    .expect("the manifest");
    tokio::fs::write(dir.join("index.js"), heard(&control))
        .await
        .expect("the entry");

    let (code, out, error) = home.run(&["-p", "say hi", "--model", "alpha/m"], "").await;
    assert_eq!(code, 0, "out: {out} error: {error}");
    assert!(
        tokio::fs::metadata(&control).await.is_ok(),
        "the plugin with no role heard run_start: {error}"
    );
    assert!(
        tokio::fs::metadata(&home.events).await.is_err(),
        "a normal run loaded a gateway plugin"
    );
}

/// The folder of session `id` under the sessions of `state`.
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
fn session_dir(state: &std::path::Path, id: &str) -> PathBuf {
    let root = reagent_store::paths::sessions_root(state);
    std::fs::read_dir(&root)
        .expect("the sessions root")
        .flatten()
        .map(|folder| folder.path().join(id))
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| panic!("no folder of session {id} under {}", root.display()))
}

/// Two answers of one question never both write: an answer holds the session's
/// answer lock from its live check to its write, so `reagent answer` waits while
/// another answer (the gateway's) holds it.
#[tokio::test]
#[expect(clippy::disallowed_types, reason = "the test holds the lock file")]
async fn gateway_an_answer_waits_for_the_answer_lock_of_the_session() {
    let home = Home::new("").await;
    let (session, entry) = parked(&home).await;
    settle(&home).await;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(session_dir(&home.state, &session).join("answer.lock"))
        .expect("the lock file");
    lock.lock().expect("the test takes the lock");

    let mut answer = Command::new(BIN)
        .env("XDG_CONFIG_HOME", &home.config)
        .env("XDG_CACHE_HOME", "/nonexistent/reagent-test-cache")
        .env("XDG_STATE_HOME", &home.state)
        .args(["answer", &entry.qid, "--no-resume"])
        .current_dir(&home.work)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the answer starts");
    let mut input = answer.stdin.take().expect("piped stdin");
    input
        .write_all(br#"[["sqlite"]]"#)
        .await
        .expect("writes the answers");
    drop(input);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        answer.try_wait().expect("the answer runs").is_none(),
        "the answer wrote while another answer held the lock"
    );
    assert!(
        !home.records(&session).await.contains("A: sqlite"),
        "no answer is written while the lock is held"
    );

    lock.unlock().expect("the test drops the lock");
    let out = tokio::time::timeout(WAIT, answer.wait_with_output())
        .await
        .expect("the answer ends in time")
        .expect("the answer runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An answer to a session a live worker holds is `busy`: the question stays
/// open, and the same answer after the worker is gone closes it.
#[tokio::test]
#[expect(clippy::disallowed_types, reason = "the test holds a part's lock")]
async fn gateway_an_answer_to_a_live_session_is_busy() {
    let home = Home::new("").await;
    let (session, entry) = parked(&home).await;
    settle(&home).await;
    let dir = session_dir(&home.state, &session);
    let part = reagent_store::paths::part_ids(&dir)
        .expect("the parts")
        .into_iter()
        .max()
        .expect("a part");
    let live =
        std::fs::File::open(reagent_store::paths::part_path(&dir, part)).expect("the part file");
    live.lock()
        .expect("the test holds the session as a live worker would");

    let mut gateway = home.gateway();
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_parked"))
        .await;
    let token = event(&told, "question_parked")["token"]
        .as_str()
        .expect("a token")
        .to_string();
    let answer = serde_json::json!({
        "qid": entry.qid,
        "token": token,
        "answers": [["sqlite"]],
        "by": "ray",
    });
    home.answer(answer.clone()).await;
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "answer"))
        .await;
    assert_eq!(event(&told, "answer")["verdict"], "busy", "{told:?}");
    assert_eq!(home.parked().await.len(), 1, "a busy question stays open");
    assert!(
        !home.records(&session).await.contains("A: sqlite"),
        "no answer is written to a live session"
    );

    live.unlock().expect("the test lets the session go");
    let mut again = answer;
    again["again"] = Value::Bool(true);
    home.answer(again).await;
    let told = home
        .told_until(|told| {
            told.iter()
                .filter(|entry| entry["event"] == "answer")
                .count()
                == 2
        })
        .await;
    let verdicts: Vec<&Value> = told
        .iter()
        .filter(|entry| entry["event"] == "answer")
        .map(|entry| &entry["verdict"])
        .collect();
    assert_eq!(verdicts[1], "ok", "{told:?}");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A run of the home's config that waits for the approval of `touch ran.txt`,
/// with `a` attached, then parks when `a` leaves: the run's session and its
/// parked entry.
async fn parked_approval(home: &Home) -> (String, reagent_protocol::discovery::Parked) {
    script(
        &home.state,
        r#"{"replies":[{"calls":[{"name":"bash","arguments":{"command":"touch ran.txt"}}]},{"text":"never read"}]}"#,
    )
    .await;
    let worker = Worker::start_with(
        &home.state,
        &home.work,
        &home.config,
        &["--model", "alpha/m"],
    )
    .await;
    let session = worker.session().to_string();
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    a.until(|frame| frame["method"] == "approval_request").await;
    drop(a);
    let entry = home.parked_until().await;
    let mut worker = worker;
    worker.kill().await;
    (session, entry)
}

/// The verdicts of the answers the plugin logged, in order.
fn verdicts(told: &[Value]) -> Vec<&Value> {
    told.iter()
        .filter(|entry| entry["event"] == "answer")
        .map(|entry| &entry["verdict"])
        .collect()
}

/// A parked approval reaches the plugin with the call it asks about, and the
/// `once` it gives back runs the call in the run that goes on.
#[tokio::test]
async fn gateway_a_parked_approval_is_given_by_a_plugin() {
    let home = Home::new("").await;
    let (session, entry) = parked_approval(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();

    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "approval_parked"))
        .await;
    let parked = event(&told, "approval_parked").clone();
    assert_eq!(parked["qid"], entry.qid.as_str());
    assert_eq!(parked["session"], session.as_str());
    assert_eq!(parked["tool"], "bash");
    assert_eq!(parked["call"], "bash: touch ran.txt");
    assert_eq!(parked["save"], "Bash(touch ran.txt:*)");
    assert!(
        !told.iter().any(|entry| entry["event"] == "question_parked"),
        "an approval is no question: {told:?}"
    );
    let token = parked["token"].as_str().expect("a token").to_string();

    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": token,
        "approve": "once",
        "by": "ray",
    }))
    .await;
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "approval_closed"))
        .await;
    assert_eq!(verdicts(&told), ["ok"], "{told:?}");
    let closed = event(&told, "approval_closed");
    assert_eq!(closed["how"], "answered");
    assert_eq!(closed["by"], "gateway:telegram:ray");

    home.records_until(&session, "done").await;
    assert!(home.work.join("ran.txt").exists(), "the approved call ran");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A parked approval takes only an approval: the answers of a question, a word
/// that is not one of the four, and a reason with a yes are denied, and the
/// approval stays open.
#[tokio::test]
async fn gateway_a_wrong_approval_answer_is_denied() {
    let home = Home::new("").await;
    home.contacts("[bob]\nchannel = \"telegram\"\nuser = \"42\"\n")
        .await;
    let (_, entry) = parked_approval(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "approval_parked"))
        .await;
    let token = event(&told, "approval_parked")["token"]
        .as_str()
        .expect("a token")
        .to_string();
    let wrong = [
        serde_json::json!({"qid": entry.qid, "token": token, "answers": [["yes"]], "by": "ray"}),
        serde_json::json!({"qid": entry.qid, "token": token, "approve": "skip", "by": "ray"}),
        serde_json::json!({"qid": entry.qid, "token": token, "approve": "once", "by": "ray", "reason": "why not"}),
        serde_json::json!({"qid": entry.qid, "token": token, "forward": "bob"}),
    ];
    for (at, ask) in wrong.into_iter().enumerate() {
        home.answer(ask).await;
        let told = home.told_until(|told| verdicts(told).len() == at + 1).await;
        assert_eq!(verdicts(&told)[at], "denied", "{told:?}");
    }
    assert_eq!(home.parked().await.len(), 1, "the approval stays open");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// An approval nobody gives is refused at its deadline: silence is never a
/// yes, so the call does not run and the run goes on with the refusal.
#[tokio::test]
async fn gateway_a_deadline_refuses_the_approval() {
    let home = Home::new("[gateway]\ndeadline = \"2s\"\n").await;
    let (session, entry) = parked_approval(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();

    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "approval_closed"))
        .await;
    let closed = event(&told, "approval_closed").clone();
    assert_eq!(closed["qid"], entry.qid.as_str());
    assert_eq!(closed["how"], "expired");
    assert_eq!(closed["by"], "deadline");

    let records = home.records_until(&session, "done").await;
    assert!(
        records.contains("nobody answered the approval request before its deadline"),
        "the model reads why the call did not run: {records}"
    );
    assert!(
        !home.work.join("ran.txt").exists(),
        "the refused call did not run"
    );
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// The contacts of the forward tests: Bob, whom the `mail` plugin reaches.
const BOB: &str = "[bob]\nname = \"Bob\"\nchannel = \"mail\"\nuser = \"42\"\n";

/// The token of the `question_parked` the owner's plugin heard.
async fn owner_token(home: &Home) -> String {
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_parked"))
        .await;
    event(&told, "question_parked")["token"]
        .as_str()
        .expect("a token")
        .to_string()
}

/// The owner forwards a parked question to a contact: only the plugin of that
/// contact hears it, with a token of the contact's own that no other plugin can
/// use, and the contact's answer reaches the model with who answered and who
/// forwarded it.
#[tokio::test]
async fn gateway_a_forwarded_question_is_answered_by_the_contact() {
    let home = Home::new("").await;
    home.contacts(BOB).await;
    let (mail, mail_answers) = home.second("mail").await;
    let (session, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();
    let token = owner_token(&home).await;

    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": token,
        "forward": "bob",
        "note": "Bob owns the budget",
    }))
    .await;
    let told = home.told_until(|told| verdicts(told).len() == 1).await;
    assert_eq!(verdicts(&told)[0], "ok", "{told:?}");
    let heard = told_until_in(&mail, |told| {
        told.iter()
            .any(|entry| entry["event"] == "question_forwarded")
    })
    .await;
    let forwarded = event(&heard, "question_forwarded").clone();
    assert_eq!(forwarded["qid"], entry.qid.as_str());
    assert_eq!(forwarded["hop"], 1);
    assert_eq!(forwarded["to"], "bob");
    assert_eq!(forwarded["name"], "Bob");
    assert_eq!(forwarded["user"], "42");
    assert_eq!(forwarded["from"], "the owner");
    assert_eq!(forwarded["note"], "Bob owns the budget");
    assert_eq!(forwarded["questions"][0]["q"], "Which database?");
    assert!(forwarded.get("session").is_none(), "{forwarded}");
    let bob = forwarded["token"].as_str().expect("a token").to_string();
    assert_eq!(bob.len(), 32, "{bob}");
    assert_ne!(bob, token, "the contact has a token of their own");
    let parked = home.parked().await;
    assert_eq!(parked[0].forwards.len(), 1, "the entry keeps the forward");

    // The owner's plugin never hears the forward, and cannot use its token.
    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": bob,
        "answers": [["postgres"]],
        "by": "bob",
    }))
    .await;
    let told = home.told_until(|told| verdicts(told).len() == 2).await;
    assert_eq!(verdicts(&told)[1], "denied", "{told:?}");
    assert!(
        !told
            .iter()
            .any(|entry| entry["event"] == "question_forwarded"),
        "{told:?}"
    );

    answer_in(
        &mail_answers,
        serde_json::json!({
            "qid": entry.qid,
            "token": bob,
            "answers": [["postgres"]],
            "by": "bob",
        }),
    )
    .await;
    let heard = told_until_in(&mail, |told| verdicts(told).len() == 1).await;
    assert_eq!(verdicts(&heard)[0], "ok", "{heard:?}");
    let told = home
        .told_until(|told| told.iter().any(|entry| entry["event"] == "question_closed"))
        .await;
    assert_eq!(event(&told, "question_closed")["by"], "gateway:mail:bob");

    let records = home.records_until(&session, "done").await;
    assert!(
        records.contains(
            r#"A: postgres\nAnswered by Bob, forwarded by the owner with the note \"Bob owns the budget\"."#
        ),
        "the model reads who answered: {records}"
    );
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A forward to a person who is not in the contacts file, or whose channel no
/// gateway plugin is, is denied and leaves the question as it was.
#[tokio::test]
async fn gateway_a_forward_to_an_unknown_contact_is_denied() {
    let home = Home::new("").await;
    home.contacts(BOB).await;
    let (_, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();
    let token = owner_token(&home).await;

    for (at, to) in ["carol", "bob"].into_iter().enumerate() {
        home.answer(serde_json::json!({"qid": entry.qid, "token": token, "forward": to}))
            .await;
        let told = home.told_until(|told| verdicts(told).len() == at + 1).await;
        assert_eq!(verdicts(&told)[at], "denied", "{to}: {told:?}");
    }
    let parked = home.parked().await;
    assert_eq!(parked.len(), 1, "the question stays open");
    assert!(parked[0].forwards.is_empty(), "no forward is kept");
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// The owner's token stays good after a forward, so the first answer wins: a
/// restarted gateway tells the contact again with a new token, the owner
/// answers, and the contact's answer after that is stale.
#[tokio::test]
async fn gateway_the_first_answer_of_a_forwarded_question_wins() {
    let home = Home::new("").await;
    home.contacts(BOB).await;
    let (mail, mail_answers) = home.second("mail").await;
    let (session, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();
    let token = owner_token(&home).await;
    home.answer(serde_json::json!({"qid": entry.qid, "token": token, "forward": "bob"}))
        .await;
    let told = home.told_until(|told| verdicts(told).len() == 1).await;
    assert_eq!(verdicts(&told)[0], "ok", "{told:?}");
    let heard = told_until_in(&mail, |told| {
        told.iter()
            .any(|entry| entry["event"] == "question_forwarded")
    })
    .await;
    let first = event(&heard, "question_forwarded")["token"].clone();
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");

    // A plugin starts its log again with the gateway, so a new token tells the
    // two starts apart; the old request must not be sent a second time.
    tokio::fs::remove_file(&home.answers)
        .await
        .expect("the old request");
    let mut gateway = home.gateway();
    let heard = told_until_in(&mail, |told| {
        told.iter()
            .any(|entry| entry["event"] == "question_forwarded" && entry["token"] != first)
    })
    .await;
    let again = event(&heard, "question_forwarded");
    assert_eq!(again["hop"], 1, "{heard:?}");
    assert_eq!(again["from"], "the owner", "{heard:?}");
    let bob = again["token"].as_str().expect("a token").to_string();
    let told = home
        .told_until(|told| {
            told.iter().any(|entry| {
                entry["event"] == "question_parked" && entry["token"] != token.as_str()
            })
        })
        .await;
    let owner = event(&told, "question_parked")["token"]
        .as_str()
        .expect("a token")
        .to_string();

    home.answer(serde_json::json!({
        "qid": entry.qid,
        "token": owner,
        "answers": [["sqlite"]],
        "by": "ray",
    }))
    .await;
    let told = home.told_until(|told| verdicts(told).len() == 1).await;
    assert_eq!(verdicts(&told)[0], "ok", "{told:?}");
    answer_in(
        &mail_answers,
        serde_json::json!({
            "qid": entry.qid,
            "token": bob,
            "answers": [["postgres"]],
            "by": "bob",
        }),
    )
    .await;
    let heard = told_until_in(&mail, |told| verdicts(told).len() == 1).await;
    assert_eq!(verdicts(&heard)[0], "stale", "{heard:?}");

    let records = home.records_until(&session, "done").await;
    assert!(records.contains("A: sqlite"), "{records}");
    assert!(
        !records.contains("Answered by"),
        "the owner's answer names nobody: {records}"
    );
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}

/// A forwarded person may forward the question again: the next forward names
/// that person as `from`, and the forwards of one question stop at 16.
#[tokio::test]
async fn gateway_a_contact_forwards_again_and_the_hops_stop_at_sixteen() {
    let home = Home::new("").await;
    home.contacts(&format!(
        "{BOB}\n[carol]\nname = \"Carol\"\nchannel = \"mail\"\nuser = \"7\"\n"
    ))
    .await;
    let (mail, mail_answers) = home.second("mail").await;
    let (_, entry) = parked(&home).await;
    settle(&home).await;
    let mut gateway = home.gateway();
    let token = owner_token(&home).await;

    home.answer(serde_json::json!({"qid": entry.qid, "token": token, "forward": "bob"}))
        .await;
    let heard = told_until_in(&mail, |told| {
        told.iter()
            .any(|entry| entry["event"] == "question_forwarded")
    })
    .await;
    let bob = event(&heard, "question_forwarded")["token"].clone();
    answer_in(
        &mail_answers,
        serde_json::json!({"qid": entry.qid, "token": bob, "forward": "carol", "note": "ask Carol"}),
    )
    .await;
    let heard = told_until_in(&mail, |told| {
        told.iter()
            .any(|entry| entry["event"] == "question_forwarded" && entry["hop"] == 2)
    })
    .await;
    assert_eq!(verdicts(&heard)[0], "ok", "{heard:?}");
    let carol = heard
        .iter()
        .find(|entry| entry["event"] == "question_forwarded" && entry["hop"] == 2)
        .expect("the second forward");
    assert_eq!(carol["from"], "Bob", "{carol}");
    assert_eq!(carol["to"], "carol", "{carol}");
    assert_eq!(carol["user"], "7", "{carol}");
    assert_eq!(carol["note"], "ask Carol", "{carol}");

    // Hops 3 to 16 are taken; the 17th is denied.
    for hop in 3..=17 {
        home.answer(serde_json::json!({
            "qid": entry.qid,
            "token": token,
            "forward": "bob",
            "note": format!("hop {hop}"),
        }))
        .await;
        let at = hop - 1;
        let told = home.told_until(|told| verdicts(told).len() == at).await;
        let want = if hop <= 16 { "ok" } else { "denied" };
        assert_eq!(verdicts(&told)[at - 1], want, "hop {hop}: {told:?}");
    }
    let parked = home.parked().await;
    assert_eq!(parked[0].forwards.len(), 16, "{:?}", parked[0].forwards);
    let (code, error) = gateway.term().await;
    assert_eq!(code, 0, "{error}");
}
