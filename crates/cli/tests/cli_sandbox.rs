//! End-to-end tests for the bash sandbox. The sandbox cannot start everywhere
//! (not inside another sandbox, not in a plain container), so each test checks
//! only what holds whether or not it started.
#![cfg(feature = "testing")]

use std::path::Path;

use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
const FAKE_KEY: &str = "sk-ant-test-sentinel-1234567890abcdef";

/// Runs a scripted two-turn session where the model calls bash with `command`.
/// `config` is the global config.toml text, if any. The run happens in
/// `<work>/work`.
async fn run_bash(work: &Path, command: &str, config: Option<&str>) -> std::process::Output {
    let script = work.join("script.json");
    let text = serde_json::json!({
        "replies": [
            {"text": "Running it.", "calls": [
                {"name": "bash", "arguments": {"command": command}}
            ]},
            {"text": "Done."}
        ]
    });
    tokio::fs::write(&script, text.to_string())
        .await
        .expect("write script");
    let xdg = work.join("xdg");
    tokio::fs::create_dir_all(xdg.join("reagent"))
        .await
        .expect("make config folder");
    if let Some(config) = config {
        tokio::fs::write(xdg.join("reagent/config.toml"), config)
            .await
            .expect("write config");
    }
    let work_dir = work.join("work");
    tokio::fs::create_dir_all(&work_dir)
        .await
        .expect("make work folder");
    Command::new(BIN)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("ANTHROPIC_API_KEY", FAKE_KEY)
        .args(["--provider", &format!("fake:{}", script.display())])
        .args(["-p", "run it"])
        .current_dir(&work_dir)
        .output()
        .await
        .expect("the binary runs")
}

fn assert_ended_normally(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn sandbox_on_never_writes_outside() {
    let work = tempfile::tempdir().expect("temp dir");
    let outside = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sandbox-outside.txt");
    let _ = tokio::fs::remove_file(&outside).await;
    let command = format!("echo x > '{}'", outside.display());
    let output = run_bash(work.path(), &command, Some("[sandbox]\nmode = \"on\"\n")).await;
    assert_ended_normally(&output);
    assert!(!outside.exists(), "the sandbox let a write through");
}

#[tokio::test]
async fn sandbox_auto_still_runs_commands() {
    let work = tempfile::tempdir().expect("temp dir");
    let output = run_bash(
        work.path(),
        "echo ok > made.txt",
        Some("[sandbox]\nmode = \"auto\"\n"),
    )
    .await;
    assert_ended_normally(&output);
    assert!(work.path().join("work/made.txt").exists());
}

#[tokio::test]
async fn sandbox_off_is_unchanged() {
    let work = tempfile::tempdir().expect("temp dir");
    let output = run_bash(work.path(), "echo ok > made.txt", None).await;
    assert_ended_normally(&output);
    assert!(work.path().join("work/made.txt").exists());
}

#[tokio::test]
async fn sandbox_helper_runs_a_program() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--write"])
        .arg(tmp.path())
        .args(["--", "sh", "-c", "echo hi"])
        .output()
        .await
        .expect("the binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(0) => assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi"),
        Some(125) => assert!(
            stderr.starts_with("reagent: sandbox unavailable:"),
            "{stderr}"
        ),
        other => panic!("exit {other:?} stderr {stderr}"),
    }
}
