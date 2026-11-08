//! End-to-end test: `reagent -p` runs a scripted FakeProvider task headless and
//! prints only the final assistant text.
//!
//! These tests drive the binary with a scripted fake provider, so they exist
//! only when the `testing` feature is on.
#![cfg(feature = "testing")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// Returns the path of the built binary for a headless run.
fn bin() -> &'static str {
    BIN
}

/// Runs `reagent` with the given arguments and returns its output. The state
/// folder is a `state` folder in `tmp`, which the test deletes at its end.
async fn run(tmp: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.join("state"))
        .env_remove("REAGENT_PROFILE")
        .args(args)
        .current_dir(tmp)
        .output()
        .await
        .expect("the binary runs")
}

#[tokio::test]
async fn cli_print_runs_a_scripted_task_and_prints_the_final_text() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = tmp.path().join("script.json");
    // Two turns: the first calls the real `write` tool, the second is the final
    // answer. The file proves the tool ran end to end.
    tokio::fs::write(
        &script,
        r#"{
          "replies": [
            {"text": "I will write the file.", "calls": [
                {"name": "write", "arguments": {"path": "out.txt", "content": "done"}}
            ]},
            {"text": "wrote out.txt"}
          ]
        }"#,
    )
    .await
    .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let output = run(
        tmp.path(),
        &["--provider", &script_arg, "-p", "write the file"],
    )
    .await;

    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // Plain mode: stdout is only the final assistant text.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "wrote out.txt\n",
        "stdout should hold only the final text"
    );
    // The `write` tool ran in the temp cwd and produced the file.
    let written = tokio::fs::read_to_string(tmp.path().join("out.txt"))
        .await
        .expect("the write tool ran");
    assert_eq!(written, "done");
}

async fn script(tmp: &Path, body: &str) -> String {
    let path = tmp.join("script.json");
    tokio::fs::write(&path, body).await.expect("write script");
    format!("fake:{}", path.display())
}

const SLOW_SCRIPT: &str = r#"{"replies": [{"calls": [
    {"name": "bash", "arguments": {"command": "sleep 30"}}
]}]}"#;

/// Runs `reagent --provider fake:<f> -p -` with `input` piped to its stdin,
/// and the state folder a `state` folder in `tmp`.
async fn run_with_stdin(tmp: &Path, fake: &str, input: &[u8]) -> std::process::Output {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new(bin())
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.join("state"))
        .args(["--provider", fake, "-p", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(input)
        .await
        .expect("writes stdin");
    drop(child.stdin.take());
    child.wait_with_output().await.expect("the run completes")
}

#[tokio::test]
async fn a_dash_task_reads_the_task_from_stdin() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), r#"{"replies": [{"text": "from stdin"}]}"#).await;
    let output = run_with_stdin(tmp.path(), &fake, b"write the file\n").await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "from stdin\n");
}

#[tokio::test]
async fn an_empty_stdin_task_exits_2() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), r#"{"replies": [{"text": "x"}]}"#).await;
    let output = run_with_stdin(tmp.path(), &fake, b"").await;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("reagent: ") && stderr.contains("empty"),
        "stderr: {stderr}"
    );
}

#[tokio::test]
async fn a_non_utf8_stdin_task_exits_2() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), r#"{"replies": [{"text": "x"}]}"#).await;
    let output = run_with_stdin(tmp.path(), &fake, &[0xff, 0xfe, 0x00]).await;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("reagent: ") && stderr.contains("UTF-8"),
        "stderr: {stderr}"
    );
}

#[tokio::test]
async fn bad_args_exit_2() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = run(tmp.path(), &["--bogus"]).await;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("reagent: "));
}

#[tokio::test]
async fn a_missing_cwd_exits_2() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), r#"{"replies": [{"text": "x"}]}"#).await;
    let output = run(
        tmp.path(),
        &[
            "--provider",
            &fake,
            "--cwd",
            "/nonexistent/reagent",
            "-p",
            "hi",
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
}

#[tokio::test]
async fn a_missing_key_exits_2() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = Command::new(bin())
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .args(["-p", "hi"])
        .current_dir(tmp.path())
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .await
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "reagent: missing ANTHROPIC_API_KEY\n"
    );
}

#[tokio::test]
async fn the_deadline_exits_124() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), SLOW_SCRIPT).await;
    let start = std::time::Instant::now();
    let output = run(
        tmp.path(),
        &["--provider", &fake, "--deadline", "1", "-p", "hi"],
    )
    .await;
    assert_eq!(output.status.code(), Some(124));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn ctrl_c_exits_130() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), SLOW_SCRIPT).await;
    let child = Command::new(bin())
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .args(["--provider", &fake, "-p", "hi"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("the binary runs");
    let pid = child.id().expect("pid").to_string();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let killed = Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .await
        .expect("kill runs");
    assert!(killed.success());
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("the run stops after Ctrl-C")
        .expect("the binary exits");
    assert_eq!(output.status.code(), Some(130));
}

#[tokio::test]
async fn a_plain_run_error_goes_to_stderr() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(
        tmp.path(),
        r#"{"replies": [{"text": "going to call", "calls": [
            {"name": "read", "arguments": {"path": "nope.txt"}}
        ]}]}"#,
    )
    .await;
    let output = run(tmp.path(), &["--provider", &fake, "-p", "hi"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("reagent: "), "{stderr}");
}

/// A script of more tool calls than a run has turns.
async fn endless_script(dir: &std::path::Path) -> String {
    tokio::fs::write(dir.join("a"), "x\n")
        .await
        .expect("write a");
    let reply = r#"{"calls": [{"name": "read", "arguments": {"path": "a"}}]}"#;
    let replies = vec![reply; 201].join(",");
    script(dir, &format!(r#"{{"replies": [{replies}]}}"#)).await
}

/// The limit stops the run with no answer, so stderr is all it has to say.
#[tokio::test]
async fn a_plain_run_says_why_it_stopped_at_the_turn_limit() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = endless_script(tmp.path()).await;
    let output = run(tmp.path(), &["--provider", &fake, "-p", "hi"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("limit of 200 turns"), "{stderr}");
    assert!(stderr.contains("--resume"), "{stderr}");
}

/// The last allowed turn answers with a handoff, and the run is still a stop at
/// the limit.
#[tokio::test]
async fn a_plain_run_prints_the_handoff_at_the_turn_limit() {
    let tmp = tempfile::tempdir().expect("temp dir");
    tokio::fs::write(tmp.path().join("a"), "x\n")
        .await
        .expect("write a");
    let call = r#"{"calls": [{"name": "read", "arguments": {"path": "a"}}]}"#;
    let mut replies = vec![call; 199];
    replies.push(r#"{"text": "status text"}"#);
    let replies = replies.join(",");
    let fake = script(tmp.path(), &format!(r#"{{"replies": [{replies}]}}"#)).await;
    let output = run(tmp.path(), &["--provider", &fake, "-p", "hi"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status text"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("limit of 200 turns"), "{stderr}");
}

#[tokio::test]
async fn a_plain_run_says_why_it_stopped_at_the_deadline() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(
        tmp.path(),
        r#"{"replies": [{"calls": [{"name": "bash", "arguments": {"command": "sleep 30"}}]}]}"#,
    )
    .await;
    let output = run(
        tmp.path(),
        &["--provider", &fake, "--deadline", "1", "-p", "hi"],
    )
    .await;
    assert_eq!(output.status.code(), Some(124));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("deadline"), "{stderr}");
}

/// A clean end has nothing to explain.
#[tokio::test]
async fn a_plain_run_says_why_it_stopped_only_when_it_did_not_finish() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), r#"{"replies": [{"text": "ok"}]}"#).await;
    let output = run(tmp.path(), &["--provider", &fake, "-p", "hi"]).await;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("stopped"), "{stderr}");
}

/// JSON mode reports the reason in its summary and keeps stderr as it was.
#[tokio::test]
async fn a_json_run_leaves_says_why_it_stopped_to_its_summary() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = endless_script(tmp.path()).await;
    let output = run(tmp.path(), &["--provider", &fake, "--json", "-p", "hi"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("limit of"), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("max_turns"), "the summary names the reason");
}

#[tokio::test]
async fn the_final_text_is_the_last_turn_only() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(
        tmp.path(),
        r#"{"replies": [
            {"text": "first look", "calls": [{"name": "read", "arguments": {"path": "a"}}]},
            {"text": "the answer"}
        ]}"#,
    )
    .await;
    tokio::fs::write(tmp.path().join("a"), "x\n")
        .await
        .expect("write a");
    let output = run(tmp.path(), &["--provider", &fake, "-p", "hi"]).await;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "the answer\n");
}

#[tokio::test]
async fn start_line_a_fake_run_names_the_profile_and_no_credential() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let fake = script(tmp.path(), r#"{"replies": [{"text": "ok"}]}"#).await;
    let output = run(tmp.path(), &["--provider", &fake, "-p", "hi"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr {stderr}");
    let line = stderr
        .lines()
        .find(|line| line.starts_with("reagent: profile "))
        .unwrap_or_else(|| panic!("no start line in {stderr}"));
    assert!(line.starts_with("reagent: profile none, model "), "{line}");
    assert!(line.ends_with(", credential none"), "{line}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
}

#[tokio::test]
async fn start_line_names_the_profile_a_run_reads() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let config = tmp.path().join("config/reagent");
    tokio::fs::create_dir_all(config.join("profiles"))
        .await
        .expect("config dir");
    let file = config.join("config.toml");
    tokio::fs::write(&file, "model = \"fake-model\"\n")
        .await
        .expect("write the config");
    tokio::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .await
        .expect("restrict the config");
    let profile = config.join("profiles/work.toml");
    tokio::fs::write(&profile, "max_tokens = 1000\n")
        .await
        .expect("write the profile");
    tokio::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600))
        .await
        .expect("restrict the profile");
    let fake = script(tmp.path(), r#"{"replies": [{"text": "ok"}]}"#).await;
    let output = Command::new(bin())
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("HOME", tmp.path())
        .env_remove("REAGENT_PROFILE")
        .args(["--provider", &fake, "--profile", "work", "-p", "hi"])
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr {stderr}");
    assert!(
        stderr
            .lines()
            .any(|line| line == "reagent: profile work, model fake-model, credential none"),
        "stderr {stderr}"
    );
}

const CHAT_REPLY: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

/// Serves one chat request with [`CHAT_REPLY`], and returns the base URL and
/// the authorization header the request carried.
async fn serve_chat() -> (String, tokio::sync::oneshot::Receiver<Option<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port");
    let address = listener.local_addr().expect("the bound address");
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept a connection");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            buffer.extend_from_slice(&chunk[..read]);
            if read == 0 || buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buffer).into_owned();
        let authorization = head.lines().find_map(|line| {
            let (name, value) = line.split_once(": ")?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.to_owned())
        });
        let _ = sender.send(authorization);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{CHAT_REPLY}",
            CHAT_REPLY.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{address}/v1"), receiver)
}

#[tokio::test]
async fn start_line_names_the_env_var_and_never_the_key() {
    const KEY: &str = "sk-print-start-line-secret";
    let (base, request) = serve_chat().await;
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = Command::new(bin())
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        .env("HOME", tmp.path())
        .env("PRINT_START_KEY", KEY)
        .env_remove("REAGENT_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "print-model",
            "--api-key-env",
            "PRINT_START_KEY",
            "--base-url",
            &base,
            "-p",
            "hi",
        ])
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr {stderr}");
    assert!(
        stderr.lines().any(|line| line
            == "reagent: profile none, model print-model, credential env PRINT_START_KEY"),
        "stderr {stderr}"
    );
    assert_eq!(stdout, "ok\n");
    assert!(!stdout.contains(KEY), "stdout leaks the key: {stdout}");
    assert!(!stderr.contains(KEY), "stderr leaks the key: {stderr}");
    // The key reached the request, so the line named its source rather than a
    // login that held nothing.
    assert_eq!(
        request.await.expect("the request was recorded"),
        Some(format!("Bearer {KEY}"))
    );
}

#[tokio::test]
async fn a_retried_turn_prints_only_the_text_of_the_last_attempt() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = tmp.path().join("script.json");
    // The first attempt streams text and is cut off; the retry succeeds.
    tokio::fs::write(
        &script,
        r#"{"replies": [{"text": "half ", "fail": "overloaded"}, {"text": "whole answer"}]}"#,
    )
    .await
    .expect("write script");
    let script_arg = format!("fake:{}", script.display());
    let output = Command::new(BIN)
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .args(["--provider", &script_arg, "-p", "hi"])
        .current_dir(tmp.path())
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .output()
        .await
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "whole answer\n");
}
