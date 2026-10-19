//! End-to-end test: a max_tokens stop with only thinking is recovered with a
//! note, and the run finishes normally.
//!
//! These tests drive the binary with a scripted fake provider, so they exist
//! only when the `testing` feature is on.
#![cfg(feature = "testing")]

use std::path::Path;

use serde_json::Value;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
const NOTE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/max_tokens.md"
));

async fn run(tmp: &Path, script: &str) -> std::process::Output {
    let path = tmp.join("script.json");
    tokio::fs::write(&path, script).await.expect("write script");
    let provider = format!("fake:{}", path.display());
    Command::new(BIN)
        .args(["--json", "--provider", &provider, "-p", "hi"])
        .current_dir(tmp)
        .env("ANTHROPIC_API_KEY", "test-key")
        .output()
        .await
        .expect("the binary runs")
}

fn json_lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect()
}

fn count(lines: &[Value], kind: &str) -> usize {
    lines.iter().filter(|line| line["type"] == kind).count()
}

#[tokio::test]
async fn max_tokens_recovery_finishes_after_a_thinking_only_reply() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = run(
        tmp.path(),
        r#"{"replies": [{"max_tokens": true}, {"text": "done"}]}"#,
    )
    .await;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = json_lines(&output.stdout);
    assert_eq!(count(&lines, "turn_start"), 2, "{lines:?}");
    let notes: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"] == "policy_note")
        .collect();
    assert_eq!(notes.len(), 1, "{lines:?}");
    assert_eq!(notes[0]["text"], NOTE);
}

#[tokio::test]
async fn max_tokens_recovery_gives_up_after_three_notes() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let reply = r#"{"max_tokens": true}"#;
    let script = format!(r#"{{"replies": [{reply}, {reply}, {reply}, {reply}]}}"#);
    let output = run(tmp.path(), &script).await;

    assert_eq!(output.status.code(), Some(1));
    let lines = json_lines(&output.stdout);
    assert_eq!(count(&lines, "turn_start"), 4, "{lines:?}");
    assert_eq!(count(&lines, "policy_note"), 3, "{lines:?}");
    let summary = lines.last().expect("a summary line");
    assert_eq!(summary["stop"], "max_tokens", "{summary}");
}
