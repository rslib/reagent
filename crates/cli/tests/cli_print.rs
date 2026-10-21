//! End-to-end test: `reagent -p` runs a scripted FakeProvider task headless and
//! prints only the final assistant text.
//!
//! These tests drive the binary with a scripted fake provider, so they exist
//! only when the `testing` feature is on.
#![cfg(feature = "testing")]

use std::path::Path;

use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// Returns the path of the built binary for a headless run.
fn bin() -> &'static str {
    BIN
}

/// Runs `reagent` with the given arguments and returns its output.
async fn run(tmp: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
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

/// Runs `reagent --provider fake:<f> -p -` with `input` piped to its stdin.
async fn run_with_stdin(fake: &str, input: &[u8]) -> std::process::Output {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new(bin())
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
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
    let output = run_with_stdin(&fake, b"write the file\n").await;
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
    let output = run_with_stdin(&fake, b"").await;
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
    let output = run_with_stdin(&fake, &[0xff, 0xfe, 0x00]).await;
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
