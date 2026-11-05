//! Cancelling a call, stopping the server, a missing command and a ping.

use crate::common;

use std::time::{Duration, Instant};

use common::{Fake, gone};
use reagent_mcp::{Client, Error, StdioSpec};
use serde_json::json;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// A line past the 16 MiB cap this client reads.
const HUGE_MIB: usize = 17;

#[tokio::test]
async fn stdio_cancel_sends_cancelled() {
    let fake = Fake::new(json!({"tools": [{"name": "sleep"}]}));
    let client = Client::stdio(&fake.spec).await.expect("connects");
    let cancel = CancellationToken::new();
    let watcher = cancel.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(100)).await;
        watcher.cancel();
    });

    let start = Instant::now();
    let result = client.call("sleep", r#"{"ms":5000}"#, &cancel).await;
    let elapsed = start.elapsed();
    let error = result.expect_err("a cancelled call");
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");

    let call_id = fake.method("tools/call").await["id"].clone();
    let cancelled = fake
        .wait_for(|message| message["method"] == "notifications/cancelled")
        .await
        .expect("a cancelled notification");
    assert_eq!(cancelled["params"]["requestId"], call_id);
    client.stop().await;
}

#[tokio::test]
async fn stdio_stop_kills_the_group() {
    let fake = Fake::new(json!({"tools": [{"name": "echo"}], "spawn_child": true}));
    let client = Client::stdio(&fake.spec).await.expect("connects");
    let server = fake.pid("server_pid").await;
    let child = fake.pid("child_pid").await;
    assert!(common::alive(server), "the server is not running");
    assert!(common::alive(child), "the child is not running");

    client.stop().await;
    assert!(gone(server).await, "the server is still running");
    assert!(gone(child).await, "the child is still running");
}

#[tokio::test]
async fn stdio_spawn_failure_names_the_command() {
    let spec = StdioSpec {
        command: "reagent-mcp-no-such-command".to_owned(),
        ..StdioSpec::default()
    };
    let error = Client::stdio(&spec).await.expect_err("a spawn error");
    assert!(
        error.to_string().contains("reagent-mcp-no-such-command"),
        "{error}"
    );
}

#[tokio::test]
async fn stdio_server_ping_is_answered() {
    let fake = Fake::new(json!({"era": "modern", "ping_before_tools_list": true}));
    let client = Client::stdio(&fake.spec).await.expect("connects");
    assert!(!client.tools().await.expect("lists tools").is_empty());
    let answer = fake
        .wait_for(|message| message["result"] == json!({}))
        .await
        .expect("a reply to the ping");
    assert!(answer["id"].is_number(), "{answer}");
    client.stop().await;
}

#[tokio::test]
async fn stdio_dead_server_fails_a_call() {
    let fake = Fake::new(json!({"tools": [{"name": "echo"}]}));
    let client = Client::stdio(&fake.spec).await.expect("connects");
    let server = fake.pid("server_pid").await;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(server),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kills the fake server");
    assert!(gone(server).await, "the fake server is still running");

    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        client.call("echo", "{}", &CancellationToken::new()),
    )
    .await;
    let elapsed = start.elapsed();
    let error = result
        .expect("the call returns within 1 s")
        .expect_err("the server is gone");
    let text = error.to_string();
    assert!(text.contains("is gone"), "{text}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    client.stop().await;
}

#[tokio::test]
async fn stdio_line_past_the_cap_fails_the_server() {
    let fake = Fake::new(json!({"huge_line_mib": HUGE_MIB}));
    let error = Client::stdio(&fake.spec)
        .await
        .expect_err("the line is past the cap")
        .to_string();
    assert!(error.contains("16 MiB"), "{error}");
}
