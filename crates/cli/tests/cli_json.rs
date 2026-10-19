//! End-to-end test: `--json` prints one line per event with usage and cache hit
//! rate, and never prints the API key.
//!
//! These tests drive the binary with a scripted fake provider, so they exist
//! only when the `testing` feature is on.
#![cfg(feature = "testing")]

use std::path::Path;

use serde_json::Value;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
/// A distinctive sentinel that must never appear in any output.
const FAKE_KEY: &str = "sk-ant-test-sentinel-1234567890abcdef";

async fn run(tmp: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .current_dir(tmp)
        .env("ANTHROPIC_API_KEY", FAKE_KEY)
        .output()
        .await
        .expect("the binary runs")
}

/// Parses every non-empty stdout line as JSON.
fn json_lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect()
}

fn types(lines: &[Value]) -> Vec<&str> {
    lines
        .iter()
        .map(|line| line["type"].as_str().expect("each line has a type"))
        .collect()
}

#[tokio::test]
async fn cli_json_prints_one_line_per_event_with_usage() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = tmp.path().join("script.json");
    // A two-turn script: a tool call with usage, then a final answer with usage.
    tokio::fs::write(
        &script,
        r#"{
          "replies": [
            {"text": "I will read the file.", "usage": {
                "input": 10, "output": 5, "cache_read": 4, "cache_write": 6, "reasoning": 0
            }, "calls": [
                {"name": "read", "arguments": {"path": "notes.txt"}}
            ]},
            {"text": "Done.", "usage": {
                "input": 12, "output": 3, "cache_read": 10, "cache_write": 2, "reasoning": 0
            }}
          ]
        }"#,
    )
    .await
    .expect("write script");
    tokio::fs::write(tmp.path().join("notes.txt"), "hello\n")
        .await
        .expect("write notes");

    let script_arg = format!("fake:{}", script.display());
    let output = run(
        tmp.path(),
        &["--json", "--provider", &script_arg, "-p", "read the file"],
    )
    .await;

    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.as_ref();
    let lines = json_lines(&output.stdout);
    let types = types(&lines);

    // Every event type the loop can emit appeared at least once, plus the
    // summary line.
    for required in [
        "turn_start",
        "text_delta",
        "tool_start",
        "tool_call_ready",
        "tool_result",
        "usage",
        "turn_end",
        "run_end",
        "summary",
    ] {
        assert!(
            types.contains(&required),
            "missing {required}; got {types:?}"
        );
    }

    // The final line is the summary with total usage and the exit reason.
    let summary = lines.last().expect("a summary line");
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["exit"], "end_turn");
    assert_eq!(summary["input"], 22, "{summary}");
    assert_eq!(summary["output"], 8, "{summary}");

    // Every usage line carries the cache hit rate: cache_read / (input +
    // cache_read + cache_write). For the first reply: 4 / (10+4+6) = 0.2.
    let usage_lines: Vec<_> = lines
        .iter()
        .filter(|line| line["type"] == "usage")
        .collect();
    assert_eq!(usage_lines.len(), 2, "one usage line per turn");
    let first = &usage_lines[0];
    assert_eq!(first["input"], 10);
    assert_eq!(first["cache_read"], 4);
    assert_eq!(first["cache_write"], 6);
    let expected = 4.0 / (10.0 + 4.0 + 6.0);
    let rate = first["cache_hit_rate"].as_f64().unwrap();
    assert!((rate - expected).abs() < 1e-9, "rate {rate} vs {expected}");

    // The fake key is never echoed, on stdout or stderr.
    assert!(
        !stdout.contains(FAKE_KEY),
        "stdout leaks the API key: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(FAKE_KEY),
        "stderr leaks the API key: {stderr}"
    );
}

#[tokio::test]
async fn cli_json_tool_call_ready_carries_name_and_arguments() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        r#"{"replies": [{"text": "calling", "calls": [
            {"name": "read", "arguments": {"path": "notes.txt"}}
        ]}, {"text": "done"}]}"#,
    )
    .await
    .expect("write script");
    tokio::fs::write(tmp.path().join("notes.txt"), "n")
        .await
        .expect("write notes");

    let script_arg = format!("fake:{}", script.display());
    let output = run(
        tmp.path(),
        &["--json", "--provider", &script_arg, "-p", "read it"],
    )
    .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let lines = json_lines(&output.stdout);
    let ready: Vec<_> = lines
        .iter()
        .filter(|line| line["type"] == "tool_call_ready")
        .collect();
    assert_eq!(ready.len(), 1, "{lines:?}");
    let ready = ready[0];
    assert_eq!(ready["name"], "read", "{ready:?}");
    // The arguments are the raw JSON string the model sent, not a parsed object.
    assert_eq!(
        ready["arguments"].as_str().unwrap(),
        r#"{"path":"notes.txt"}"#,
        "arguments stay a raw JSON string: {ready:?}"
    );
}

#[tokio::test]
async fn cli_json_never_prints_the_api_key_on_an_error() {
    let tmp = tempfile::tempdir().expect("temp dir");
    // One reply calls a tool and leaves nothing to answer a second turn, so the
    // run ends in error (empty reply) with a non-zero exit. The key must not
    // leak in the error line or the exit path.
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        r#"{"replies": [{"text": "going to call", "calls": [
            {"name": "read", "arguments": {"path": "nope.txt"}}
        ]}]}"#,
    )
    .await
    .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let output = run(
        tmp.path(),
        &["--json", "--provider", &script_arg, "-p", "hi"],
    )
    .await;

    // A failed run is exit 1 (not 0); stdout may carry an error event line.
    assert_eq!(
        output.status.code(),
        Some(1),
        "{} output",
        String::from_utf8_lossy(&output.stdout)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(FAKE_KEY),
        "stdout leaks the API key: {stdout}"
    );
    assert!(
        !stderr.contains(FAKE_KEY),
        "stderr leaks the API key: {stderr}"
    );
}

#[tokio::test]
async fn cli_json_completion_policy_prints_a_policy_note() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = tmp.path().join("script.json");
    // The first answer gets the completion note; the second answer ends the run.
    tokio::fs::write(
        &script,
        r#"{"replies": [{"text": "first answer"}, {"text": "checked answer"}]}"#,
    )
    .await
    .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let args = [
        "--json",
        "--policy",
        "completion",
        "--provider",
        &script_arg,
        "-p",
        "hi",
    ];
    let output = run(tmp.path(), &args).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = json_lines(&output.stdout);
    let kinds = types(&lines);
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "policy_note").count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "turn_start").count(),
        2,
        "{kinds:?}"
    );
}

#[tokio::test]
async fn cli_json_without_json_flag_prints_plain_final_text() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = tmp.path().join("script.json");
    tokio::fs::write(&script, r#"{"replies": [{"text": "plain answer"}]}"#)
        .await
        .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let output = run(tmp.path(), &["--provider", &script_arg, "-p", "hi"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "plain answer\n");
}

#[tokio::test]
async fn cli_json_a_transport_error_never_prints_the_api_key() {
    let tmp = tempfile::tempdir().expect("temp dir");
    // The real Anthropic provider against a closed local port: the request
    // fails, and neither the error line nor stderr may hold the key.
    let output = Command::new(BIN)
        .args(["--json", "-p", "hi"])
        .current_dir(tmp.path())
        .env("ANTHROPIC_API_KEY", FAKE_KEY)
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .output()
        .await
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        types(&json_lines(&output.stdout)).contains(&"error"),
        "{stdout}"
    );
    assert!(
        !stdout.contains(FAKE_KEY),
        "stdout leaks the API key: {stdout}"
    );
    assert!(
        !stderr.contains(FAKE_KEY),
        "stderr leaks the API key: {stderr}"
    );
}
