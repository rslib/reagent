//! The ACP v1 session methods: `session/list`, `session/resume`, `session/close`, `session/delete` and `$/cancel_request`.

use super::*;

impl Acp {
    /// Runs one prompt to its answer and returns the lines it made, its answer
    /// last.
    async fn run_prompt(&mut self, session: &str, text: &str) -> Vec<Value> {
        let id = self.prompt(session, text).await;
        self.until(|line| line["id"].as_u64() == Some(id)).await
    }
}

/// The model select of a session's `configOptions`, from the answer of a
/// `session/new`, a `session/load`, a `session/resume` or a
/// `session/set_config_option`.
fn model_option(result: &Value) -> &Value {
    result["configOptions"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["id"] == "model"))
        .unwrap_or_else(|| panic!("a model option in {result}"))
}

/// The `sessions` of the answer to a `session/list`.
fn listed(answer: &Value) -> &[Value] {
    answer["result"]["sessions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("a list of sessions: {answer}"))
}

/// The file the discovery entry of the worker of `session` is written to.
fn worker_entry(state: &Path, session: &str) -> PathBuf {
    state
        .join("reagent")
        .join("workers")
        .join("v1")
        .join(format!("{session}.json"))
}

/// Waits until the discovery entry of the worker of `session` is gone, which it
/// is once the worker stopped.
async fn worker_gone(state: &Path, session: &str) {
    let entry = worker_entry(state, session);
    for _ in 0..300 {
        if !entry.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "the worker of {session} still has its entry at {}",
        entry.display()
    );
}

/// `initialize` admits the four session methods of this agent.
#[tokio::test]
async fn acp_session_capabilities_are_advertised() {
    let state = state();
    let cwd = work(state.path()).await;
    let mut acp = Acp::start(state.path(), &cwd, &[]).await;

    let answer = acp.initialize().await;
    let agent = &answer["result"]["agentCapabilities"];
    assert_eq!(agent["loadSession"], true, "{answer}");
    let session = &agent["sessionCapabilities"];
    for name in ["list", "resume", "close", "delete"] {
        assert_eq!(session[name], json!({}), "{name} in {answer}");
    }
}

/// `session/list` names the sessions of a folder, newest first, with the title
/// of each and the time it was last written.
#[tokio::test]
async fn acp_list_shows_the_sessions_of_a_folder() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"one"},{"text":"two"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;

    let first = acp.new_session(&cwd).await;
    acp.run_prompt(&first, "first task").await;
    let second = acp.new_session(&cwd).await;
    acp.run_prompt(&second, "second task").await;

    let id = acp
        .request("session/list", json!({"cwd": path(&cwd)}))
        .await;
    let answer = acp.answer(id).await;
    let sessions = listed(&answer);
    assert_eq!(sessions.len(), 2, "{answer}");
    assert_eq!(sessions[0]["sessionId"], second.as_str(), "{answer}");
    assert_eq!(sessions[1]["sessionId"], first.as_str(), "{answer}");
    assert_eq!(sessions[0]["cwd"], path(&cwd), "{answer}");
    assert_eq!(sessions[1]["cwd"], path(&cwd), "{answer}");
    assert_eq!(sessions[0]["title"], "second task", "{answer}");
    assert_eq!(sessions[1]["title"], "first task", "{answer}");
    assert!(
        sessions[0]["updatedAt"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z') && at.len() == 20),
        "the time is ISO 8601: {answer}"
    );
    assert!(answer["result"]["nextCursor"].is_null(), "{answer}");

    // No `cwd` names every folder, each session with the folder it ran in.
    let id = acp.request("session/list", json!({})).await;
    let answer = acp.answer(id).await;
    let all = listed(&answer);
    for session in [first.as_str(), second.as_str()] {
        let found = all
            .iter()
            .find(|listed| listed["sessionId"] == session)
            .unwrap_or_else(|| panic!("{session} in {answer}"));
        assert_eq!(found["cwd"], path(&cwd), "{answer}");
    }

    // A folder with no session lists none.
    let other = state.path().join("other");
    tokio::fs::create_dir(&other).await.expect("the folder");
    let id = acp
        .request("session/list", json!({"cwd": path(&other)}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(listed(&answer).len(), 0, "{answer}");

    // A cursor this agent never wrote is invalid.
    let id = acp
        .request(
            "session/list",
            json!({"cwd": path(&cwd), "cursor": "not-a-cursor"}),
        )
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
}

/// `session/resume` joins a session without replaying it, and the session takes
/// a prompt afterwards.
#[tokio::test]
async fn acp_resume_answers_with_no_replay() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"hello from reagent"},{"text":"again"}]}"#,
    )
    .await;
    let mut first = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    first.initialize().await;
    let session = first.new_session(&cwd).await;
    let lines = first.run_prompt(&session, "say hi").await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "end_turn",
        "{:?}",
        lines.last()
    );

    let mut second = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    second.initialize().await;
    let id = second
        .request(
            "session/resume",
            json!({"sessionId": session, "cwd": path(&cwd), "mcpServers": []}),
        )
        .await;
    let lines = second.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert!(answer.get("error").is_none(), "{answer}");
    model_option(&answer["result"]);
    assert!(
        answer["result"]["modes"]["currentModeId"].is_string(),
        "{answer}"
    );
    assert_eq!(lines.len(), 1, "nothing is replayed: {lines:?}");

    let lines = second.run_prompt(&session, "again").await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "end_turn",
        "{:?}",
        lines.last()
    );
    assert!(
        chunks(&lines, "agent_message_chunk").contains("again"),
        "the reply is streamed: {lines:?}"
    );
}

/// `session/resume` of a session no folder holds is a missing session, and one
/// of a folder that is not there is invalid params.
#[tokio::test]
async fn acp_resume_refuses_a_session_that_is_not_there() {
    let state = state();
    let cwd = work(state.path()).await;
    let mut acp = Acp::start(state.path(), &cwd, &[]).await;
    acp.initialize().await;

    let id = acp
        .request(
            "session/resume",
            json!({"sessionId": "notasession", "cwd": path(&cwd), "mcpServers": []}),
        )
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32002, "{answer}");
    assert_eq!(
        answer["error"]["data"]["sessionId"], "notasession",
        "{answer}"
    );

    let id = acp
        .request(
            "session/resume",
            json!({"sessionId": "notasession", "cwd": "/nonexistent/reagent-acp"}),
        )
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
}

/// `session/close` ends the task of a session and its worker, and a prompt to
/// it finds no session afterwards.
#[tokio::test]
async fn acp_close_ends_the_task_and_its_worker() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"one"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    assert!(
        worker_entry(state.path(), &session).exists(),
        "a session this agent serves has a worker"
    );

    let id = acp
        .request("session/close", json!({"sessionId": session}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"], json!({}), "{answer}");

    let id = acp.prompt(&session, "anyone home").await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32002, "{answer}");

    worker_gone(state.path(), &session).await;

    // A close of a session this agent no longer holds is the same empty result.
    let id = acp
        .request("session/close", json!({"sessionId": session}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"], json!({}), "{answer}");
}

/// A close of a session with a prompt waiting ends that prompt `cancelled`
/// first, and then answers the close.
#[tokio::test]
async fn acp_close_cancels_the_prompt_it_holds() {
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

    let prompt = acp.prompt(&session, "run it").await;
    acp.until(|line| line["method"] == "session/request_permission")
        .await;
    let id = acp
        .request("session/close", json!({"sessionId": session}))
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answered: Vec<u64> = lines
        .iter()
        .filter_map(|line| line["id"].as_u64())
        .collect();
    assert_eq!(answered, [prompt, id], "the prompt first: {lines:?}");
    assert_eq!(
        lines[0]["result"]["stopReason"], "cancelled",
        "{:?}",
        lines[0]
    );
    assert_eq!(lines[1]["result"], json!({}), "{:?}", lines[1]);
}

/// `session/delete` removes the folder of a stopped session, and refuses one
/// whose worker still runs.
#[tokio::test]
async fn acp_delete_removes_a_stopped_session() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"one"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.run_prompt(&session, "first task").await;

    let id = acp
        .request("session/delete", json!({"sessionId": session}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");

    let id = acp
        .request("session/close", json!({"sessionId": session}))
        .await;
    acp.answer(id).await;
    worker_gone(state.path(), &session).await;

    let id = acp
        .request("session/delete", json!({"sessionId": session}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"], json!({}), "{answer}");

    let id = acp
        .request("session/list", json!({"cwd": path(&cwd)}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(listed(&answer).len(), 0, "the session is gone: {answer}");

    // A session that is already gone is deleted silently.
    let id = acp
        .request("session/delete", json!({"sessionId": session}))
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"], json!({}), "{answer}");
}

/// `$/cancel_request` ends the prompt it names with `cancelled`, and an id that
/// names nothing leaves the session alone.
#[tokio::test]
async fn acp_cancel_request_ends_the_prompt_it_names() {
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
    acp.notify("$/cancel_request", json!({"requestId": id}))
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "cancelled",
        "{:?}",
        lines.last()
    );

    // An id that names no request of this agent is ignored.
    acp.notify("$/cancel_request", json!({"requestId": 987_654}))
        .await;
    let lines = acp.run_prompt(&session, "again").await;
    assert_eq!(
        lines.last().expect("an answer")["result"]["stopReason"],
        "end_turn",
        "{:?}",
        lines.last()
    );
    assert!(
        chunks(&lines, "agent_message_chunk").contains("done"),
        "the session still runs: {lines:?}"
    );
}

/// A folder of the sessions root that links outside it must not let a delete
/// remove the session at the link's target.
#[cfg(unix)]
#[tokio::test]
async fn acp_delete_does_not_follow_a_folder_symlink() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"one"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.run_prompt(&session, "keep this session").await;
    let close = acp
        .request("session/close", json!({"sessionId": session.clone()}))
        .await;
    acp.answer(close).await;
    worker_gone(state.path(), &session).await;

    let keyed =
        reagent_store::paths::sessions_root(state.path()).join(reagent_store::paths::cwd_key(&cwd));
    let outside = state.path().join("outside-sessions-root");
    tokio::fs::rename(&keyed, &outside).await.unwrap();
    std::os::unix::fs::symlink(&outside, &keyed).unwrap();
    let target = outside.join(&session);
    assert!(target.exists());

    let id = acp
        .request("session/delete", json!({"sessionId": session}))
        .await;
    let answer = acp.answer(id).await;
    assert!(
        target.exists(),
        "session/delete escaped the sessions root: {answer}"
    );
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
}

/// A session folder that is itself a link is refused, and what it points at
/// stays.
#[cfg(unix)]
#[tokio::test]
async fn acp_delete_refuses_a_symlinked_session_folder() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"one"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.run_prompt(&session, "keep this session").await;
    let close = acp
        .request("session/close", json!({"sessionId": session.clone()}))
        .await;
    acp.answer(close).await;
    worker_gone(state.path(), &session).await;

    let keyed =
        reagent_store::paths::sessions_root(state.path()).join(reagent_store::paths::cwd_key(&cwd));
    let dir = keyed.join(&session);
    let outside = state.path().join("outside-session");
    tokio::fs::rename(&dir, &outside).await.unwrap();
    std::os::unix::fs::symlink(&outside, &dir).unwrap();

    let id = acp
        .request("session/delete", json!({"sessionId": session}))
        .await;
    let answer = acp.answer(id).await;
    assert!(outside.exists(), "the link target was removed: {answer}");
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
}

/// The FIFO keeps the blocking read of the list pending until after the cancel
/// notification, which then answers the list as cancelled.
#[cfg(unix)]
#[tokio::test]
async fn acp_cancel_request_answers_a_pending_list() {
    use tokio::io::AsyncWriteExt;

    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"one"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.run_prompt(&session, "keep this session").await;
    let close = acp
        .request("session/close", json!({"sessionId": session.clone()}))
        .await;
    acp.answer(close).await;
    worker_gone(state.path(), &session).await;

    let keyed =
        reagent_store::paths::sessions_root(state.path()).join(reagent_store::paths::cwd_key(&cwd));
    let list = keyed.join("sessions.list");
    tokio::fs::remove_file(&list).await.unwrap();
    let made = tokio::process::Command::new("mkfifo")
        .arg(&list)
        .status()
        .await
        .unwrap();
    assert!(made.success());

    let id = acp
        .request("session/list", json!({"cwd": path(&cwd)}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    acp.notify("$/cancel_request", json!({"requestId": id}))
        .await;

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut writer = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&list)
            .await
            .unwrap();
        writer.write_all(b"x").await.unwrap();
    });
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32800, "{answer}");
}

/// A `$/cancel_request` of a pending `session/delete` does not cancel it: the
/// delete runs and answers its own result, never `-32800`.
#[cfg(unix)]
#[tokio::test]
async fn acp_cancel_request_does_not_cancel_a_delete() {
    use tokio::io::AsyncWriteExt;

    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"one"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.run_prompt(&session, "keep this session").await;
    let close = acp
        .request("session/close", json!({"sessionId": session.clone()}))
        .await;
    acp.answer(close).await;
    worker_gone(state.path(), &session).await;

    let keyed =
        reagent_store::paths::sessions_root(state.path()).join(reagent_store::paths::cwd_key(&cwd));
    let list = keyed.join("sessions.list");
    tokio::fs::remove_file(&list).await.unwrap();
    let made = tokio::process::Command::new("mkfifo")
        .arg(&list)
        .status()
        .await
        .unwrap();
    assert!(made.success());

    let id = acp
        .request("session/delete", json!({"sessionId": session}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    acp.notify("$/cancel_request", json!({"requestId": id}))
        .await;

    // The delete reads the list, which waits for a writer, and then writes it
    // again, which waits for a reader: a writer and a reader come in turn until
    // the delete is done.
    let peer = tokio::spawn(async move {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let Ok(mut writer) = tokio::fs::OpenOptions::new().write(true).open(&list).await else {
                return;
            };
            let _ = writer.write_all(b"x").await;
            drop(writer);
            let Ok(mut reader) = tokio::fs::File::open(&list).await else {
                return;
            };
            let mut sink = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut sink).await;
        }
    });
    let answer = acp.answer(id).await;
    peer.abort();
    assert_ne!(answer["error"]["code"], -32800, "{answer}");
}
