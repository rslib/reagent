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
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
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
async fn cli_json_completion_policy_notes_unchecked_changes() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let work = tempfile::tempdir().expect("work dir");
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        r#"{"replies": [{"text": "writing", "calls": [
            {"name": "write", "arguments": {"path": "out.txt", "content": "hi"}}
        ]}, {"text": "first answer"}, {"text": "checked answer"}]}"#,
    )
    .await
    .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let work_arg = work.path().display().to_string();
    let args = [
        "--json",
        "--policy",
        "completion",
        "--cwd",
        &work_arg,
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
    let notes: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"] == "policy_note")
        .collect();
    assert_eq!(notes.len(), 1, "{:?}", types(&lines));
    assert!(
        notes[0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("You changed files")),
        "{:?}",
        notes[0]
    );
}

#[tokio::test]
async fn cli_json_completion_policy_is_quiet_without_changes() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let work = tempfile::tempdir().expect("work dir");
    let script = tmp.path().join("script.json");
    tokio::fs::write(&script, r#"{"replies": [{"text": "plain answer"}]}"#)
        .await
        .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let work_arg = work.path().display().to_string();
    let args = [
        "--json",
        "--policy",
        "completion",
        "--cwd",
        &work_arg,
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
    assert!(
        !types(&lines).contains(&"policy_note"),
        "{:?}",
        types(&lines)
    );
}

#[tokio::test]
async fn usage_cost_and_context_tokens_in_the_summary() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let work = tempfile::tempdir().expect("work dir");
    // A model table names the context window, so the summary can report it.
    let config = tmp.path().join("xdg/reagent");
    tokio::fs::create_dir_all(&config)
        .await
        .expect("config dir");
    tokio::fs::write(
        config.join("config.toml"),
        "[providers.fi.models.\"m\"]\ncontext_window = 200000\n",
    )
    .await
    .expect("write config");
    let script = tmp.path().join("script.json");
    // Two turns: the first calls a tool, so the second is the last turn, and
    // its prompt is the one that sizes the context.
    tokio::fs::write(
        &script,
        r#"{"replies": [
            {"text": "first", "usage": {
                "input": 40, "output": 5, "cache_read": 0, "cache_write": 0, "reasoning": 0
            }, "calls": [
                {"name": "read", "arguments": {"path": "notes.txt"}}
            ]},
            {"text": "second", "usage": {
                "input": 12, "output": 3, "cache_read": 10, "cache_write": 2, "reasoning": 0
            }}
        ]}"#,
    )
    .await
    .expect("write script");
    tokio::fs::write(work.path().join("notes.txt"), "hello\n")
        .await
        .expect("write notes");

    let work_arg = work.path().display().to_string();
    let output = Command::new(BIN)
        .args(["--json", "--model", "fi/m", "--cwd", &work_arg])
        .arg("--provider")
        .arg(format!("fake:{}", script.display()))
        .args(["-p", "hi"])
        .current_dir(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        .env("ANTHROPIC_API_KEY", FAKE_KEY)
        .output()
        .await
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lines = json_lines(&output.stdout);
    let summary = lines.last().expect("a summary line");
    assert_eq!(summary["type"], "summary");
    // The scripted usage reports no cost, so the summary has none.
    assert!(
        summary.get("cost").is_some(),
        "the summary names the cost: {summary}"
    );
    assert_eq!(summary["cost"], Value::Null, "{summary}");
    // The context is the last turn's prompt: input + cache_read + cache_write.
    assert_eq!(summary["context_tokens"], 24, "{summary}");
    assert_eq!(summary["context_window"], 200_000, "{summary}");

    // Every usage line names the cost too, as null when the provider has none.
    let usage_lines: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"] == "usage")
        .collect();
    assert_eq!(usage_lines.len(), 2, "{:?}", types(&lines));
    for line in usage_lines {
        assert_eq!(line["cost"], Value::Null, "{line}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(FAKE_KEY), "stdout leaks the API key");
}

#[tokio::test]
async fn usage_cost_comes_from_the_cached_price() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let work = tempfile::tempdir().expect("work dir");
    // A table makes `fi` a provider name, so `fi/m` finds its cache entry.
    let config = tmp.path().join("xdg/reagent");
    tokio::fs::create_dir_all(&config)
        .await
        .expect("config dir");
    tokio::fs::write(config.join("config.toml"), "[providers.fi]\n")
        .await
        .expect("write config");
    let models = tmp.path().join("cache/reagent/models");
    tokio::fs::create_dir_all(&models).await.expect("cache dir");
    tokio::fs::write(
        models.join("fi.json"),
        r#"{"fetched_at": 0, "models": [{"id": "m", "context_window": 8000, "max_output": null,
            "price": {"input": 0.000001, "output": 0.000002, "cache_read": 0.0, "cache_write": 0.0}}]}"#,
    )
    .await
    .expect("write cache");
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        r#"{"replies": [{"text": "done", "usage": {
            "input": 1000, "output": 500, "cache_read": 0, "cache_write": 0, "reasoning": 0
        }}]}"#,
    )
    .await
    .expect("write script");

    let work_arg = work.path().display().to_string();
    let output = Command::new(BIN)
        .args(["--json", "--model", "fi/m", "--cwd", &work_arg])
        .arg("--provider")
        .arg(format!("fake:{}", script.display()))
        .args(["-p", "hi"])
        .current_dir(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        .env("ANTHROPIC_API_KEY", FAKE_KEY)
        .output()
        .await
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lines = json_lines(&output.stdout);
    let usage = lines
        .iter()
        .find(|line| line["type"] == "usage")
        .expect("a usage line");
    let cost = usage["cost"].as_f64().expect("a computed cost");
    assert!((cost - 0.002).abs() < 1e-12, "{usage}");
    let summary = lines.last().expect("a summary line");
    let total = summary["cost"].as_f64().expect("a summary cost");
    assert!((total - 0.002).abs() < 1e-12, "{summary}");
    assert_eq!(summary["context_window"], 8000, "{summary}");
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
    // fails, and neither the retry lines nor stderr may hold the key. The
    // deadline ends the run during the retry backoff, which lasts minutes.
    let output = Command::new(BIN)
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .args(["--json", "-p", "hi", "--deadline", "3"])
        .current_dir(tmp.path())
        .env("ANTHROPIC_API_KEY", FAKE_KEY)
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .output()
        .await
        .expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let retry_errors: Vec<_> = json_lines(&output.stdout)
        .into_iter()
        .filter(|line| line["type"] == "retry")
        .filter_map(|line| line["error"].as_str().map(str::to_owned))
        .collect();
    assert!(
        retry_errors
            .iter()
            .any(|error| error.contains("http error")),
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

#[tokio::test]
async fn cli_json_prints_a_compacted_line() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let work = tempfile::tempdir().expect("work dir");
    // 70 tool turns of 4 KB each with a prompt at the budget, then an answer:
    // once 150 KB lie past the 80 KB tail, the run compacts, and only once.
    let turn = r#"{"text": "listing", "usage": {"input": 150000, "output": 1, "cache_read": 0, "cache_write": 0, "reasoning": 0}, "calls": [
        {"name": "bash", "arguments": {"command": "head -c 4000 /dev/zero | tr '\\0' x"}}
    ]}"#;
    let mut replies: Vec<&str> = vec![turn; 70];
    replies.push(r#"{"text": "done"}"#);
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        format!(r#"{{"replies": [{}]}}"#, replies.join(",")),
    )
    .await
    .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let work_arg = work.path().display().to_string();
    let args = [
        "--json",
        "--policy",
        "compact",
        "--cwd",
        &work_arg,
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
    let compacted: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"] == "compacted")
        .collect();
    assert_eq!(compacted.len(), 1, "{:?}", types(&lines));
    assert_eq!(compacted[0]["before"], 150000);
    assert!(compacted[0]["replaced"].as_u64().is_some_and(|n| n > 0));
    assert!(compacted[0]["bytes"].as_u64().is_some_and(|n| n > 0));
}

#[tokio::test]
async fn cli_json_a_denied_call_names_the_rule() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let work = tempfile::tempdir().expect("work dir");
    let config = tmp.path().join("xdg/reagent");
    tokio::fs::create_dir_all(&config)
        .await
        .expect("config dir");
    tokio::fs::write(
        config.join("config.toml"),
        "[permissions]\ndeny = [\"Bash(git push:*)\"]\n",
    )
    .await
    .expect("write config");
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        r#"{"replies": [{"text": "pushing", "calls": [
            {"name": "bash", "arguments": {"command": "echo hi > ran.txt; git push"}}
        ]}, {"text": "done"}]}"#,
    )
    .await
    .expect("write script");

    let output = Command::new(BIN)
        .args(["--json", "--cwd"])
        .arg(work.path())
        .arg("--provider")
        .arg(format!("fake:{}", script.display()))
        .args(["-p", "hi"])
        .current_dir(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env("ANTHROPIC_API_KEY", FAKE_KEY)
        .output()
        .await
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = json_lines(&output.stdout);
    let result = lines
        .iter()
        .find(|line| line["type"] == "tool_result")
        .expect("a tool result");
    assert_eq!(result["is_error"], true, "{result:?}");
    assert!(
        !work.path().join("ran.txt").exists(),
        "no part of a denied command runs"
    );
    assert!(lines.iter().any(|line| line["type"] == "run_end"));
}
