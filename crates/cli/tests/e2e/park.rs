//! A run that parks when its last client leaves, and the answer that continues
//! it from another process: `reagent answer`.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::worker_live::{Client, Worker, ask_folder, ask_script, config_of, script, state};

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
const WAIT: Duration = Duration::from_secs(60);
const MARK: Duration = Duration::from_millis(50);

/// A worker whose run asks a question, with `a` attached.
async fn asked(state: &Path) -> (Worker, Client) {
    let cwd = state.join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    ask_folder(&cwd).await;
    let provider = script(state, &ask_script()).await;
    let worker = Worker::start(state, &cwd, &["--provider", &provider]).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    let frames = a.until(|frame| frame["method"] == "question_request").await;
    assert!(
        frames
            .last()
            .is_some_and(|frame| frame["params"]["questions"][0]["q"] == "Which database?"),
        "the client hears the question: {frames:?}"
    );
    (worker, a)
}

/// The parked entries of the discovery folder of `state`, as a process that did
/// not run the session reads them.
async fn parked(state: &Path) -> Vec<reagent_protocol::discovery::Parked> {
    let folder = reagent_protocol::discovery::folder(state);
    reagent_protocol::discovery::list_parked(&folder, |_, _| true).await
}

/// Waits for the worker of the session to leave the index entry of its question.
async fn parked_until(state: &Path) -> reagent_protocol::discovery::Parked {
    for _ in 0..(WAIT.as_millis() / MARK.as_millis()) {
        if let Some(entry) = parked(state).await.pop() {
            return entry;
        }
        tokio::time::sleep(MARK).await;
    }
    panic!("the worker leaves no parked question");
}

/// Runs the binary in `cwd` with `args` and `stdin`, and returns its code, its
/// output and its errors.
async fn run(state: &Path, cwd: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    run_in(
        state,
        cwd,
        Path::new("/nonexistent/reagent-test-config"),
        args,
        stdin,
    )
    .await
}

/// Runs the binary as [`run`] does, with the config folder `config`.
async fn run_in(
    state: &Path,
    cwd: &Path,
    config: &Path,
    args: &[&str],
    stdin: &str,
) -> (i32, String, String) {
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
        .expect("the command starts");
    let mut input = child.stdin.take().expect("piped stdin");
    input
        .write_all(stdin.as_bytes())
        .await
        .expect("writes the answers");
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

/// The records of the session `session` of the folder `cwd`, as `session cat`
/// prints them.
async fn records(state: &Path, cwd: &Path, session: &str) -> String {
    let (code, out, error) = run(state, cwd, &["session", "cat", session], "").await;
    assert_eq!(code, 0, "session cat: {error}");
    out
}

/// The records of `session` once they hold `text`.
async fn records_until(state: &Path, cwd: &Path, session: &str, text: &str) -> String {
    let mut last = String::new();
    for _ in 0..(WAIT.as_millis() / MARK.as_millis()) {
        last = records(state, cwd, session).await;
        if last.contains(text) {
            return last;
        }
        tokio::time::sleep(MARK).await;
    }
    panic!("the records never held {text}: {last}");
}

/// The last client of a waiting run leaves: the run parks, leaves the question in
/// the index, and `reagent answer` lists it with its session and its question.
#[tokio::test]
async fn park_the_last_client_leaving_leaves_the_question_for_an_answer() {
    let dir = state();
    let state = dir.path();
    let (worker, a) = asked(state).await;
    let session = worker.session().to_string();
    // The last client leaves, so the run parks instead of cancelling the wait.
    drop(a);
    let entry = parked_until(state).await;
    assert_eq!(entry.session, session, "the entry names the session");
    assert_eq!(entry.first(), "Which database?");
    assert_eq!(
        entry.qid.len(),
        32,
        "the question is the one the journal holds: {}",
        entry.qid
    );
    let mut worker = worker;
    worker.kill().await;

    // The listing of the command the person runs, in the folder of the session.
    let cwd = state.join("work");
    let (code, out, error) = run(state, &cwd, &["answer"], "").await;
    assert_eq!(code, 0, "{error}");
    assert!(out.contains(&entry.qid), "the listing names the id: {out}");
    assert!(
        out.contains(&session),
        "the listing names the session: {out}"
    );
    assert!(
        out.contains("Which database?"),
        "the listing names the question: {out}"
    );
    let (code, out, error) = run(state, &cwd, &["answer", "--json"], "").await;
    assert_eq!(code, 0, "{error}");
    let listed: serde_json::Value = serde_json::from_str(&out).expect("the listing is JSON");
    assert_eq!(listed[0]["qid"], entry.qid.as_str());
    assert_eq!(listed[0]["session"], session.as_str());
}

/// An answer written by another process records the result, and the resumed
/// session goes on: the model reads the answer, the run ends its turn, and the
/// index no longer holds the question.
#[tokio::test]
async fn park_the_answer_goes_on_headless_to_the_model() {
    let dir = state();
    let state = dir.path();
    let cwd = state.join("work");
    let (worker, a) = asked(state).await;
    let session = worker.session().to_string();
    drop(a);
    let entry = parked_until(state).await;
    let mut worker = worker;
    worker.kill().await;

    // The answer goes into the journal of the session and starts nothing.
    let (code, out, error) = run(
        state,
        &cwd,
        &["answer", &entry.qid, "--no-resume"],
        r#"[["yes"]]"#,
    )
    .await;
    assert_eq!(code, 0, "{error}");
    assert!(
        out.contains(&session),
        "the command names the session: {out}"
    );
    assert!(
        parked(state).await.is_empty(),
        "the index no longer lists it"
    );

    // The session goes on with no client of its own, as `reagent answer` starts
    // it: the model reads the answer and the turn ends.
    let path = state.join("resumed.json");
    tokio::fs::write(&path, r#"{"replies":[{"text":"done"}]}"#)
        .await
        .expect("writes the script");
    let provider = format!("fake:{}", path.display());
    let resumed = Worker::start(state, &cwd, &["-r", &session, "--provider", &provider]).await;
    let token = resumed.entry().await.token;
    let mut b = Client::initialize(&resumed, &token, "b").await;
    b.attach(b.committed).await;
    let frames = b
        .until(|frame| crate::worker_live::is_event(frame, "run_end"))
        .await;
    let shown = crate::worker_live::events(frames);
    let ended = shown
        .iter()
        .rev()
        .find(|event| event["type"] == "run_end")
        .expect("the run ends");
    assert_eq!(ended["reason"], "end_turn", "{shown:?}");
    let mut resumed = resumed;
    resumed.kill().await;

    let records = records_until(state, &cwd, &session, "done").await;
    assert!(
        records.contains("A: yes"),
        "the model reads the answer in its request: {records}"
    );
    assert!(
        records.matches("turn_end").count() >= 2,
        "the turn of the answer ended too: {records}"
    );

    // The journal no longer waits for the question, and its entry is gone.
    let (code, out, error) = run(state, &cwd, &["answer"], "").await;
    assert_eq!(code, 0, "{error}");
    assert!(
        !out.contains(&entry.qid),
        "the listing drops the answered question: {out}"
    );
    let (code, _, error) = run(state, &cwd, &["answer", &entry.qid], r#"[["yes"]]"#).await;
    assert_eq!(code, 1, "{error}");
}

/// A session on a provider of the config goes on by itself after a plain
/// `reagent answer`: the worker it starts takes the provider of the session
/// from its journal, with no flag.
#[tokio::test]
async fn park_an_answer_restarts_the_session_on_its_config_provider() {
    let dir = state();
    let state = dir.path();
    let cwd = state.join("work");
    tokio::fs::create_dir(&cwd).await.expect("the work folder");
    ask_folder(&cwd).await;
    let wire = script(state, &ask_script()).await;
    let config = config_of(state, &format!("[providers.alpha]\nwire = \"{wire}\"\n")).await;
    let worker = Worker::start_with(state, &cwd, &config, &["--model", "alpha/m"]).await;
    let session = worker.session().to_string();
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    a.attach(a.committed).await;
    a.request("send", r#"{"text":"run it","mode":"queue"}"#)
        .await;
    a.until(|frame| frame["method"] == "question_request").await;
    drop(a);
    let entry = parked_until(state).await;
    let mut worker = worker;
    worker.kill().await;

    // The same provider now ends the turn, so the rerun cannot ask again.
    script(state, r#"{"replies":[{"text":"done"}]}"#).await;
    let (code, out, error) = run_in(
        state,
        &cwd,
        &config,
        &["answer", &entry.qid],
        r#"[["yes"]]"#,
    )
    .await;
    assert_eq!(code, 0, "{error}");
    assert!(
        out.contains(&session),
        "the command names the session: {out}"
    );

    let records = records_until(state, &cwd, &session, "done").await;
    assert!(
        records.contains("A: yes"),
        "the model reads the answer in its request: {records}"
    );
    assert!(
        parked(state).await.is_empty(),
        "the index no longer lists it"
    );
}

/// A restart that fails says why: a session whose provider came from a raw
/// `--provider` flag has no provider in the config to restart on, so
/// `reagent answer` keeps the answer and names the reason.
#[tokio::test]
async fn park_a_failed_restart_names_its_reason() {
    let dir = state();
    let state = dir.path();
    let cwd = state.join("work");
    let (worker, a) = asked(state).await;
    let session = worker.session().to_string();
    drop(a);
    let entry = parked_until(state).await;
    let mut worker = worker;
    worker.kill().await;

    let (code, out, error) = run(state, &cwd, &["answer", &entry.qid], r#"[["yes"]]"#).await;
    assert_eq!(code, 1, "out: {out} error: {error}");
    assert!(
        error.contains("recorded") && error.contains(&session),
        "the answer is kept and the session named: {error}"
    );
    assert!(
        error.contains("ANTHROPIC_API_KEY"),
        "the worker's own reason is shown: {error}"
    );
    // The answer stays in the journal: a second answer is refused.
    let (code, _, error) = run(state, &cwd, &["answer", &entry.qid], r#"[["no"]]"#).await;
    assert_eq!(code, 1, "{error}");
}

/// An answer with the wrong number of lists writes nothing.
#[tokio::test]
async fn park_a_wrong_answer_count_records_nothing() {
    let dir = state();
    let state = dir.path();
    let (worker, a) = asked(state).await;
    let session = worker.session().to_string();
    drop(a);
    let entry = parked_until(state).await;
    let mut worker = worker;
    worker.kill().await;

    let cwd = state.join("work");
    let before = records(state, &cwd, &session).await;
    let (code, _, error) = run(
        state,
        &cwd,
        &["answer", &entry.qid],
        r#"[["sqlite"],["postgres"]]"#,
    )
    .await;
    assert_eq!(code, 2, "{error}");
    assert!(error.contains("answers for"), "{error}");
    let after = records(state, &cwd, &session).await;
    assert_eq!(before, after, "no record is written");
    let listed = parked(state).await;
    assert_eq!(listed.len(), 1, "the index still lists it: {listed:?}");
    assert_eq!(listed[0].qid, entry.qid);
}

/// An answer while a live worker holds the session is refused, so the wait of
/// that client is the one that answers it.
#[tokio::test]
async fn park_an_answer_of_a_live_session_is_refused() {
    let dir = state();
    let state = dir.path();
    let (worker, a) = asked(state).await;
    let session = worker.session().to_string();
    drop(a);
    let entry = parked_until(state).await;
    let mut worker = worker;
    worker.kill().await;

    // A worker of the session takes it again, with a client that stays.
    let cwd = state.join("work");
    let provider = script(state, &ask_script()).await;
    let mut live = Worker::start(state, &cwd, &["-r", &session, "--provider", &provider]).await;
    let token = live.entry().await.token;
    let mut b = Client::initialize(&live, &token, "b").await;
    b.attach(b.committed).await;

    let (code, _, error) = run(state, &cwd, &["answer", &entry.qid], r#"[["yes"]]"#).await;
    assert_eq!(code, 1, "{error}");
    assert!(
        error.contains(&format!(
            "session {session} is live; answer it in its client"
        )),
        "{error}"
    );
    assert_eq!(
        parked(state).await.len(),
        1,
        "the entry of the waiting question stays"
    );
    live.kill().await;
}
