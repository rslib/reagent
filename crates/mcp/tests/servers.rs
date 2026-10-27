//! Tests for a set of MCP servers started in the background and its adapter.

mod common;

use std::time::{Duration, Instant};

use common::Fake;
use reagent_agent::tool::Tool as _;
use reagent_config::mcp::McpDirect;
use reagent_config::{McpServer, McpTransport};
use reagent_mcp::{ServerStatus, Servers, exposed_name};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// How long a test waits for a server, longer than any fake needs.
const WAIT: Duration = Duration::from_secs(10);

/// The variable the missing-variable test asks for, which no run sets.
const MISSING: &str = "REAGENT_MCP_TEST_UNSET_VARIABLE";

/// The server `fake` starts, with `timeout` for one call.
fn server(fake: &Fake, timeout: Duration) -> McpServer {
    McpServer {
        transport: McpTransport::Stdio {
            command: fake.spec.command.clone(),
            args: fake.spec.args.clone(),
            env: fake.spec.env.clone(),
            cwd: None,
        },
        timeout,
        enabled: true,
        direct: McpDirect::None,
        oauth: None,
    }
}

/// The names `status` holds, when it is ready.
fn ready_tools(status: &ServerStatus) -> Vec<&str> {
    match status {
        ServerStatus::Ready(tools) => tools.iter().map(|tool| tool.name.as_str()).collect(),
        other => panic!("expected a ready server, got {other:?}"),
    }
}

#[tokio::test]
async fn mcp_direct_adapters() {
    let fake = Fake::new(json!({"tools": [{"name": "echo"}, {"name": "sleep"}]}));
    let mut only = server(&fake, WAIT);
    only.direct = McpDirect::Only(vec!["echo".to_string()]);
    let servers = Servers::start(&[("demo".to_string(), only)], None);
    servers.wait_ready(WAIT).await;
    let names: Vec<String> = servers
        .direct_adapters()
        .iter()
        .map(|adapter| adapter.spec().name.clone())
        .collect();
    assert_eq!(names, ["mcp__demo__echo"]);
    assert!(servers.warnings().is_empty(), "{:?}", servers.warnings());
    servers.stop().await;

    // `All` offers every tool of the server.
    let fake = Fake::new(json!({"tools": [{"name": "echo"}, {"name": "sleep"}]}));
    let mut all = server(&fake, WAIT);
    all.direct = McpDirect::All;
    let servers = Servers::start(&[("demo".to_string(), all)], None);
    servers.wait_ready(WAIT).await;
    let names: Vec<String> = servers
        .direct_adapters()
        .iter()
        .map(|adapter| adapter.spec().name.clone())
        .collect();
    assert_eq!(names, ["mcp__demo__echo", "mcp__demo__sleep"]);
    servers.stop().await;

    // A name the server does not have is a warning, and offers no adapter.
    let fake = Fake::new(json!({"tools": [{"name": "echo"}]}));
    let mut missing = server(&fake, WAIT);
    missing.direct = McpDirect::Only(vec!["nope".to_string()]);
    let servers = Servers::start(&[("demo".to_string(), missing)], None);
    servers.wait_ready(WAIT).await;
    assert!(servers.direct_adapters().is_empty());
    assert_eq!(servers.warnings(), ["mcp demo: no tool nope for direct"]);
    servers.stop().await;
}

#[tokio::test]
async fn mcp_direct_long_name() {
    let shared = "s".repeat(55);
    let first = format!("{shared}{}", "a".repeat(15));
    let second = format!("{shared}{}", "b".repeat(15));
    let mut named = Vec::new();
    for name in [first.clone(), second.clone()] {
        let fake = Fake::new(json!({"tools": [{"name": name}]}));
        let servers = Servers::start(&[("demo".to_string(), server(&fake, WAIT))], None);
        servers.wait_ready(WAIT).await;
        let names = servers.names();
        assert_eq!(names.len(), 1, "{names:?}");
        named.push(names[0].clone());
        servers.stop().await;
    }
    for name in &named {
        assert_eq!(name.len(), 64, "{name}");
        let (kept, hash) = name.rsplit_once('_').expect("a mark before the hash");
        assert_eq!(kept.len(), 55, "{name}");
        assert_eq!(hash.len(), 8, "{name}");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{name}"
        );
    }
    assert_ne!(named[0], named[1]);
    // The same tool name gives the same exposed name on every run.
    assert_eq!(exposed_name("demo", &first), named[0]);
    assert_eq!(exposed_name("demo", &first), exposed_name("demo", &first));
}

#[tokio::test]
async fn mcp_codemode_servers_start_in_background() {
    let fake = Fake::new(json!({"tools": [{"name": "echo"}, {"name": "search"}]}));
    let servers = Servers::start(&[("gh".to_string(), server(&fake, WAIT))], None);
    // `start` returned without waiting for the child to answer.
    let snapshot = servers.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].name, "gh");
    assert!(
        matches!(snapshot[0].status, ServerStatus::Starting),
        "{:?}",
        snapshot[0].status
    );

    servers.wait_ready(WAIT).await;
    let snapshot = servers.snapshot();
    assert_eq!(ready_tools(&snapshot[0].status), ["echo", "search"]);
    assert_eq!(
        servers.names(),
        ["mcp__gh__echo".to_string(), "mcp__gh__search".to_string()]
    );
    // A ready adapter keeps the run's calls going, and the stop still ends the
    // child it belongs to.
    let adapter = servers.tool("mcp__gh__echo").await.expect("an adapter");
    let pid = fake.pid("server_pid").await;
    servers.stop().await;
    assert!(common::gone(pid).await, "the server child is gone");
    assert!(adapter.run("{}", &CancellationToken::new()).await.is_error);
}

#[tokio::test]
async fn mcp_codemode_missing_env_fails_one_server() {
    let fake = Fake::new(json!(null));
    let mut broken = server(&fake, WAIT);
    broken.transport = McpTransport::Stdio {
        command: fake.spec.command.clone(),
        args: Vec::new(),
        env: vec![("TOKEN".to_string(), format!("${{{MISSING}}}"))],
        cwd: None,
    };
    let servers = Servers::start(
        &[
            ("bad".to_string(), broken),
            ("good".to_string(), server(&fake, WAIT)),
        ],
        None,
    );
    servers.wait_ready(WAIT).await;
    let snapshot = servers.snapshot();
    let failed = match &snapshot[0].status {
        ServerStatus::Failed(reason) => reason.clone(),
        other => panic!("expected a failed server, got {other:?}"),
    };
    assert!(failed.contains(MISSING), "{failed}");
    assert!(!failed.contains("TOKEN="), "{failed}");
    assert_eq!(ready_tools(&snapshot[1].status), ["echo"]);

    let error = servers.tool("mcp__bad__echo").await.expect_err("an error");
    assert!(error.contains(MISSING), "{error}");
    servers.stop().await;
}

#[tokio::test]
async fn mcp_codemode_tool_adapter_calls() {
    let fake = Fake::new(json!({"tools": [
        {"name": "echo"},
        {"name": "structured"},
        {"name": "broken", "error": "nope"},
        {"name": "reading", "annotations": {"readOnlyHint": true}, "output_schema": {"type": "object"}},
    ]}));
    let servers = Servers::start(&[("gh".to_string(), server(&fake, WAIT))], None);
    servers.wait_ready(WAIT).await;
    let cancel = CancellationToken::new();

    let echo = servers.tool("mcp__gh__echo").await.expect("an adapter");
    assert_eq!(echo.spec().name, "mcp__gh__echo");
    assert_eq!(echo.spec().description, "the echo tool");
    assert!(!echo.read_only());
    assert_eq!(echo.returns(), "string");
    let out = echo.run(r#"{"text":"hi"}"#, &cancel).await;
    assert!(!out.is_error, "{out:?}");
    assert!(out.content.contains("hi"), "{out:?}");

    let structured = servers
        .tool("mcp__gh__structured")
        .await
        .expect("an adapter");
    let out = structured.run("{}", &cancel).await;
    assert!(!out.is_error, "{out:?}");
    let object = out.object.expect("the structured content");
    assert_eq!(object.fields["b"], json!(2));

    let broken = servers.tool("mcp__gh__broken").await.expect("an adapter");
    let out = broken.run("{}", &cancel).await;
    assert!(out.is_error);
    assert_eq!(out.content.as_ref(), "nope");

    let reading = servers.tool("mcp__gh__reading").await.expect("an adapter");
    assert!(reading.read_only());
    assert_eq!(reading.returns(), "object");
    let unknown = servers.tool("mcp__gh__nope").await.expect_err("an error");
    assert!(unknown.contains("mcp__gh__nope"), "{unknown}");
    servers.stop().await;
}

#[tokio::test]
async fn mcp_codemode_slow_server_waits() {
    let fake = Fake::new(json!({"start_delay_ms": 1000}));
    let servers = Servers::start(&[("slow".to_string(), server(&fake, WAIT))], None);
    let started = Instant::now();
    let tool = servers
        .tool("mcp__slow__echo")
        .await
        .expect("the wait ends when the server answers");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "waited {:?}",
        started.elapsed()
    );
    assert_eq!(tool.spec().name, "mcp__slow__echo");
    servers.stop().await;
}

#[tokio::test]
async fn mcp_codemode_call_past_the_timeout_fails() {
    let fake = Fake::new(json!({"tools": [{"name": "sleep"}]}));
    let limit = Duration::from_secs(1);
    let servers = Servers::start(&[("slow".to_string(), server(&fake, limit))], None);
    servers.wait_ready(WAIT).await;
    let tool = servers.tool("mcp__slow__sleep").await.expect("an adapter");

    let start = Instant::now();
    let out = tool.run(r#"{"ms":4000}"#, &CancellationToken::new()).await;
    let elapsed = start.elapsed();
    assert!(out.is_error, "{out:?}");
    assert!(
        out.content
            .contains("the MCP server slow did not answer in 1 s"),
        "{out:?}"
    );
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    servers.stop().await;
}
