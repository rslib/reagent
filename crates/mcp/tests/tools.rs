//! The tool list: pages, calls, a failed call, a list change and parallel calls.

mod common;

use std::time::{Duration, Instant};

use common::Fake;
use common::http::{Server, Settings, tool};
use futures_util::future::join_all;
use reagent_agent::tool::Tool as _;
use reagent_config::mcp::McpDirect;
use reagent_config::{McpServer, McpTransport};
use reagent_mcp::{Client, HttpSpec, Servers};
use serde_json::json;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// The most a page, a call and a list change may take.
const SLOW: Duration = Duration::from_secs(3);

/// How long the fake takes to answer a `tools/list` past the first.
const LIST_DELAY_MS: u64 = 2000;

/// The most a call may take while a relist runs.
const CALL_LIMIT: Duration = Duration::from_secs(1);

/// How long the call waits before it starts, so the relist is under way.
const SETTLE: Duration = Duration::from_millis(200);

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

/// A client without a cancel token.
fn open(fake: &Fake) -> impl std::future::Future<Output = Client> {
    let spec = fake.spec.clone();
    async move { Client::stdio(&spec).await.expect("connects") }
}

#[tokio::test]
async fn tools_are_listed_across_pages() {
    let fake = Fake::new(json!({"era": "modern", "pages": 3}));
    let client = open(&fake).await;
    let tools = client.tools().await.expect("lists tools");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        ["tool-1", "tool-2", "tool-3", "tool-4", "tool-5", "tool-6"]
    );
    client.stop().await;
}

#[tokio::test]
async fn tools_call_returns_text_and_structured() {
    let fake = Fake::new(json!({"tools": [{"name": "echo"}, {"name": "structured"}]}));
    let client = open(&fake).await;
    let cancel = CancellationToken::new();

    let echoed = client
        .call("echo", r#"{"a":1}"#, &cancel)
        .await
        .expect("calls echo");
    assert_eq!(echoed.text, r#"{"a":1}"#);
    assert!(!echoed.is_error);
    assert!(echoed.structured.is_none(), "{:?}", echoed.structured);

    let structured = client
        .call("structured", "{}", &cancel)
        .await
        .expect("calls structured");
    assert_eq!(structured.structured, Some(json!({"b": 2})));
    client.stop().await;
}

#[tokio::test]
async fn tools_failed_call_is_an_error() {
    let fake = Fake::new(json!({"tools": [{"name": "fail", "error": "bad id"}]}));
    let client = open(&fake).await;
    let cancel = CancellationToken::new();
    match client.call("fail", "{}", &cancel).await {
        Ok(output) => assert!(output.is_error, "{output:?}"),
        Err(error) => assert!(error.to_string().contains("bad id"), "{error}"),
    }
    client.stop().await;
}

#[tokio::test]
async fn tools_list_changed_relists() {
    let fake = Fake::new(json!({
        "era": "legacy",
        "tools": [{"name": "echo"}],
        "list_changed_after_call": true,
    }));
    let client = open(&fake).await;
    let cancel = CancellationToken::new();
    assert_eq!(client.tools().await.expect("lists tools").len(), 1);
    client
        .call("echo", "{}", &cancel)
        .await
        .expect("calls echo");

    let start = Instant::now();
    let mut names = Vec::new();
    while start.elapsed() < SLOW {
        let tools = client.tools().await.expect("lists tools");
        if tools.len() > 1 {
            names = tools.iter().map(|tool| tool.name.clone()).collect();
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(names, ["echo", "added"]);

    let lists = fake
        .messages()
        .await
        .iter()
        .filter(|message| message["method"] == "tools/list")
        .count();
    assert!(lists > 1, "the tool list was not listed again");
    client.stop().await;
}

#[tokio::test]
async fn tools_parallel_calls() {
    let fake = Fake::new(json!({"tools": [{"name": "sleep"}]}));
    let client = open(&fake).await;
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let calls: Vec<_> = (0..8)
        .map(|_| client.call("sleep", r#"{"ms":200}"#, &cancel))
        .collect();
    let outputs = join_all(calls).await;
    let elapsed = start.elapsed();
    for output in outputs {
        assert_eq!(output.expect("calls sleep").text, "slept");
    }
    assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
    client.stop().await;
}

#[tokio::test]
async fn tools_list_relists_after_the_ttl() {
    let fake = Fake::new(json!({"era": "modern", "ttl_ms": 20}));
    let client = open(&fake).await;
    assert!(!client.tools().await.expect("lists tools").is_empty());
    sleep(Duration::from_millis(60)).await;
    assert!(!client.tools().await.expect("lists tools").is_empty());

    let lists = fake
        .messages()
        .await
        .iter()
        .filter(|message| message["method"] == "tools/list")
        .count();
    assert!(
        lists > 1,
        "the tool list was not listed again after its ttl"
    );
    client.stop().await;
}

#[tokio::test]
async fn tools_list_changed_updates_servers() {
    let fake = Fake::new(json!({
        "era": "legacy",
        "tools": [{"name": "echo"}],
        "list_changed_after_call": true,
    }));
    let servers = Servers::start(&[("gh".to_owned(), server(&fake, SLOW))], None);
    servers.wait_ready(SLOW).await;
    assert_eq!(servers.names(), ["mcp__gh__echo".to_owned()]);

    let echo = servers.tool("mcp__gh__echo").await.expect("an adapter");
    let cancel = CancellationToken::new();
    echo.run("{}", &cancel).await;

    let start = Instant::now();
    let mut names = Vec::new();
    while start.elapsed() < SLOW {
        servers.refresh().await;
        names = servers.names();
        if names.len() > 1 {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        names,
        ["mcp__gh__added".to_owned(), "mcp__gh__echo".to_owned()]
    );

    let added = servers.tool("mcp__gh__added").await.expect("the new tool");
    assert_eq!(added.spec().name, "mcp__gh__added");
    assert!(!added.run("{}", &cancel).await.is_error);
    servers.stop().await;
}

#[tokio::test]
async fn tools_call_not_blocked_by_relist() {
    let fake = Server::start(Settings {
        tools: vec![tool("echo", json!({"type": "object"}))],
        list_ttl_ms: Some(0),
        list_delay_ms: Some(LIST_DELAY_MS),
        ..Settings::default()
    })
    .await;
    let client = Client::http(&HttpSpec {
        url: fake.url.clone(),
        ..HttpSpec::default()
    })
    .await
    .expect("connects");
    let cancel = CancellationToken::new();
    assert_eq!(client.tools().await.expect("lists tools").len(), 1);

    // The relist waits its two seconds in the fake; a call must not wait for it.
    let relisting = async { client.tools().await.expect("relists tools") };
    let calling = async {
        sleep(SETTLE).await;
        let started = Instant::now();
        let output = client
            .call("echo", "{}", &cancel)
            .await
            .expect("calls echo");
        (started.elapsed(), output)
    };
    let (listed, (elapsed, output)) = tokio::join!(relisting, calling);
    assert_eq!(listed.len(), 1);
    assert_eq!(output.text, "the echo tool ran");
    assert!(
        elapsed < CALL_LIMIT,
        "the call waited {elapsed:?} for a relist"
    );
    client.stop().await;
}
