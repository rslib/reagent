//! End-to-end test: a signal ends a run and stops the MCP servers it started.
//!
//! The run needs a scripted fake provider, so this test exists only when the
//! `testing` feature is on.

#![cfg(feature = "testing")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long the test waits for the server to write its pid.
const READY_LIMIT: Duration = Duration::from_secs(10);

/// How long the run may take to exit after a signal.
const EXIT_LIMIT: Duration = Duration::from_secs(5);

/// How long the server may outlive the run.
const SERVER_LIMIT: Duration = Duration::from_secs(3);

/// How often the test looks again.
const POLL: Duration = Duration::from_millis(50);

/// The scripted provider: one call that keeps the run busy.
const SLOW: &str = r#"{"replies": [{"calls": [
    {"name": "bash", "arguments": {"command": "sleep 30"}}
]}]}"#;

/// A stdio MCP server that writes its pid to the file in `$1` and answers the
/// handshake with no tools.
const SERVER: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$1"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then continue; fi
  case "$line" in
    *'"server/discover"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}\n' "$id" ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[],"resultType":"complete","ttlMs":600000}}\n' "$id" ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no such method"}}\n' "$id" ;;
  esac
done
"#;

#[tokio::test]
async fn a_signal_ends_the_run_and_stops_the_servers() {
    for signal in ["-TERM", "-HUP"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let pid = spawn_run_and_stop(dir.path(), signal).await;
        assert!(
            gone(pid, SERVER_LIMIT).await,
            "{signal}: the server outlived the run"
        );
    }
}

/// Starts a run with one fake stdio server, sends `signal` to it once the
/// server is up, and returns the pid the server wrote.
async fn spawn_run_and_stop(dir: &Path, signal: &str) -> u32 {
    let pid_file = dir.join("server.pid");
    let script = dir.join("server.sh");
    tokio::fs::write(&script, SERVER)
        .await
        .expect("writes the server");
    tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .await
        .expect("makes the server runnable");
    let config = dir.join("config/reagent");
    tokio::fs::create_dir_all(&config)
        .await
        .expect("makes the config folder");
    let file = config.join("mcp.toml");
    tokio::fs::write(
        &file,
        format!(
            "[servers.fake]\ncommand = \"/bin/sh\"\nargs = [\"{}\", \"{}\"]\n",
            script.display(),
            pid_file.display()
        ),
    )
    .await
    .expect("writes mcp.toml");
    tokio::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .await
        .expect("makes the config private");
    let fake = dir.join("script.json");
    tokio::fs::write(&fake, SLOW)
        .await
        .expect("writes the script");
    let provider = format!("fake:{}", fake.display());
    let mut child = Command::new(BIN)
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env_remove("REAGENT_PROFILE")
        .args(["--provider", &provider, "-p", "hi"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the binary runs");
    let pid = wait_for_pid(&pid_file).await;
    assert!(alive(pid).await, "{signal}: the server did not start");
    let run = child.id().expect("the run has a pid").to_string();
    let sent = Command::new("kill")
        .args([signal, &run])
        .status()
        .await
        .expect("kill runs");
    assert!(sent.success(), "{signal}: kill failed");
    let status = tokio::time::timeout(EXIT_LIMIT, child.wait())
        .await
        .expect("the run stops after a signal")
        .expect("the binary exits");
    assert_eq!(status.code(), Some(130), "{signal}: the exit code");
    pid
}

/// The pid the fake server wrote, once it has written it.
async fn wait_for_pid(path: &Path) -> u32 {
    let began = Instant::now();
    loop {
        if let Ok(text) = tokio::fs::read_to_string(path).await {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(
            began.elapsed() < READY_LIMIT,
            "no pid in {}",
            path.display()
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Whether the process `pid` is still there.
async fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Whether the process `pid` is gone before `limit` runs out.
async fn gone(pid: u32, limit: Duration) -> bool {
    let began = Instant::now();
    while began.elapsed() < limit {
        if !alive(pid).await {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    !alive(pid).await
}
