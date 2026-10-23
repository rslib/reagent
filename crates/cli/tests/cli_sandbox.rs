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
async fn sandbox_exec_keeps_the_helper_pid() {
    // The helper hands its process to the program with `exec`, so the program
    // prints the helper's own pid, not a child's.
    let child = Command::new(BIN)
        .args(["--sandbox-exec", "--", "sh", "-c", "echo $$"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let id = child.id();
    let output = child.wait_with_output().await.expect("the binary exits");
    if unavailable(&output) {
        return;
    }
    let printed: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("a pid on stdout");
    assert_eq!(
        Some(printed),
        id,
        "the program did not keep the helper's pid"
    );

    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--", "sh", "-c", "exit 7"])
        .output()
        .await
        .expect("the binary runs");
    if unavailable(&output) {
        return;
    }
    assert_eq!(
        output.status.code(),
        Some(7),
        "the program's exit code did not reach the caller: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn sandbox_exec_keeps_the_helper_pid_on_a_signal() {
    // With `exec` a killed program kills the helper's own process, so the
    // caller sees the signal itself, as it does without a sandbox.
    use std::os::unix::process::ExitStatusExt;
    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--", "sh", "-c", "kill -9 $$"])
        .output()
        .await
        .expect("the binary runs");
    if unavailable(&output) {
        return;
    }
    assert_eq!(output.status.signal(), Some(9), "{:?}", output.status);
}

#[tokio::test]
async fn sandbox_exec_runs_a_program_copied_into_a_write_folder() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--write"])
        .arg(tmp.path())
        .args([
            "--",
            "sh",
            "-c",
            "printf '#!/bin/sh\\necho built\\n' > ./e && chmod +x ./e && ./e",
        ])
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(0) => assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "built"),
        Some(125) => assert!(
            stderr.starts_with("reagent: sandbox unavailable:"),
            "{stderr}"
        ),
        other => panic!("exit {other:?}: {stderr}"),
    }
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

/// Runs `script` with `sh -c` in the helper, allowed to write only `write`.
async fn helper(write: &Path, script: &str) -> std::process::Output {
    Command::new(BIN)
        .args(["--sandbox-exec", "--write"])
        .arg(write)
        .args(["--", "sh", "-c", script])
        .current_dir(write)
        .output()
        .await
        .expect("the binary runs")
}

/// True when the helper reported that the sandbox could not start here.
fn unavailable(output: &std::process::Output) -> bool {
    output.status.code() == Some(125)
        && String::from_utf8_lossy(&output.stderr).starts_with("reagent: sandbox unavailable:")
}

#[tokio::test]
async fn sandbox_backend_lets_a_command_signal_its_child() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let output = helper(tmp.path(), "sleep 30 & kill -INT $! && echo killed").await;
    if unavailable(&output) {
        return;
    }
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "killed",
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn sandbox_backend_refuses_a_write_outside_the_write_folders() {
    let allowed = tempfile::tempdir().expect("allowed dir");
    let other = tempfile::tempdir().expect("other dir");
    let target = other.path().join("escape.txt");
    let script = format!("echo x > '{}'; echo inside > ok.txt", target.display());
    let output = helper(allowed.path(), &script).await;
    if unavailable(&output) {
        return;
    }
    assert!(!target.exists(), "the write outside went through");
    assert!(
        allowed.path().join("ok.txt").exists(),
        "the write inside failed"
    );
}

/// The `bash` on `PATH`, for a test that names it directly.
fn bash_on_path() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("bash"))
        .find(|candidate| candidate.is_file())
}

/// A helper start with `PATH` pointing at a folder with nothing the shell
/// needs, so only a resolved shell path can run the command.
#[tokio::test]
async fn perf_helper_runs_a_resolved_shell_without_a_path_search() {
    let Some(bash) = bash_on_path() else {
        return;
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let empty = tmp.path().join("empty-path");
    tokio::fs::create_dir_all(&empty).await.expect("empty dir");
    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--shell"])
        .arg(&bash)
        .args(["--write"])
        .arg(tmp.path())
        .args(["--", "bash", "-c", "echo hi"])
        .env("PATH", &empty)
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(125) {
        // The sandbox cannot start here, but a missing shell must not be why.
        assert!(
            !stderr.contains("cannot run") && !stderr.contains("unknown sandbox argument"),
            "the helper did not use the resolved shell: {stderr}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "exit {} stderr {stderr}",
        output.status
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
}

#[tokio::test]
async fn perf_helper_a_removed_shell_falls_back_to_the_search() {
    let Some(bash) = bash_on_path() else {
        return;
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    tokio::fs::symlink(&bash, tmp.path().join("bash"))
        .await
        .expect("symlink");
    let gone = tmp.path().join("gone-bash");
    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--shell"])
        .arg(&gone)
        .args(["--write"])
        .arg(tmp.path())
        .args(["--", "bash", "-c", "echo hi"])
        .env("PATH", tmp.path())
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(125) {
        assert!(!stderr.contains("cannot run"), "{stderr}");
        return;
    }
    assert!(
        output.status.success(),
        "exit {} stderr {stderr}",
        output.status
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
}

#[tokio::test]
async fn perf_helper_falls_back_to_sh_without_a_resolved_shell() {
    let Some(bash) = bash_on_path() else {
        return;
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    tokio::fs::symlink(&bash, tmp.path().join("sh"))
        .await
        .expect("symlink");
    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--write"])
        .arg(tmp.path())
        .args(["--", "bash", "-c", "echo hi"])
        .env("PATH", tmp.path())
        .current_dir(tmp.path())
        .output()
        .await
        .expect("the binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(125) {
        assert!(!stderr.contains("cannot run"), "{stderr}");
        return;
    }
    assert!(
        output.status.success(),
        "exit {} stderr {stderr}",
        output.status
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
}

#[tokio::test]
async fn perf_helper_writes_inside_a_resolved_folder() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let dir = tokio::fs::canonicalize(tmp.path())
        .await
        .expect("a canonical path");
    let output = Command::new(BIN)
        .args(["--sandbox-exec", "--write-resolved"])
        .arg(&dir)
        .args(["--", "sh", "-c", "echo ok > made.txt"])
        .current_dir(&dir)
        .output()
        .await
        .expect("the binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(125) {
        assert!(
            !stderr.contains("unknown sandbox argument"),
            "the helper ignores a resolved folder: {stderr}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "exit {} stderr {stderr}",
        output.status
    );
    let made = tokio::fs::read_to_string(dir.join("made.txt"))
        .await
        .expect("the write inside went through");
    assert_eq!(made.trim(), "ok");
}

#[tokio::test]
async fn sandbox_backend_links_no_gpl_crate() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .await
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("metadata is JSON");
    let gpl: Vec<String> = metadata["packages"]
        .as_array()
        .expect("a package list")
        .iter()
        .filter(|package| {
            package["license"]
                .as_str()
                .is_some_and(|license| license.contains("GPL") && !license.contains("LGPL"))
        })
        .map(|package| package["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(gpl.is_empty(), "GPL crates: {gpl:?}");
}

/// Runs `bash -c script` in the helper, with the network when `network`.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
async fn helper_bash(write: &Path, network: bool, script: &str) -> std::process::Output {
    let mut command = Command::new(BIN);
    command.args(["--sandbox-exec", "--write"]).arg(write);
    if network {
        command.arg("--network");
    }
    command
        .args(["--", "bash", "-c", script])
        .current_dir(write)
        .output()
        .await
        .expect("the binary runs")
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[tokio::test]
async fn sandbox_backend_refuses_udp_without_network() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = "echo x > /dev/udp/127.0.0.1/9 && echo sent";
    let output = helper_bash(tmp.path(), false, script).await;
    if unavailable(&output) || bash_on_path().is_none() {
        return;
    }
    assert_ne!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "sent",
        "UDP went out with the network off"
    );

    let output = helper_bash(tmp.path(), true, script).await;
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "sent",
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
