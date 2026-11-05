//! The HTTP transport: era, headers, sessions, retries, cancel and env expansion.

use crate::common;

use std::time::{Duration, Instant};

use common::http::{PORT, Server, Settings, tool};
use reagent_mcp::{Client, Era, Error, HttpSpec, expand_env};
use serde_json::json;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// A client for `server` without extra headers.
async fn open(server: &Server) -> Client {
    connect(server, Vec::new()).await
}

/// A client for `server` with `headers` on every request.
async fn connect(server: &Server, headers: Vec<(String, String)>) -> Client {
    let spec = HttpSpec {
        url: server.url.clone(),
        headers,
        ..HttpSpec::default()
    };
    Client::http(&spec).await.expect("connects")
}

/// The schema of a tool with no arguments.
fn object() -> serde_json::Value {
    json!({"type": "object"})
}

/// A body past the 16 MiB cap this client reads.
const HUGE_MIB: usize = 17;

#[tokio::test]
async fn http_modern_json_reply() {
    let server = Server::start(Settings {
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    assert_eq!(client.era(), Era::Modern("2026-07-28"));
    let tools = client.tools().await.expect("lists tools");
    assert_eq!(tools.len(), 1);

    let cancel = CancellationToken::new();
    let output = client
        .call("echo", "{}", &cancel)
        .await
        .expect("calls echo");
    assert_eq!(output.text, "the echo tool ran");
    assert!(!output.is_error);

    for request in server.requests() {
        assert_eq!(request.method, "POST", "{request:?}");
        assert_eq!(
            request.header("mcp-protocol-version"),
            Some("2026-07-28"),
            "{request:?}"
        );
        assert!(request.header("mcp-method").is_some(), "{request:?}");
        assert_eq!(
            request.header("content-type"),
            Some("application/json"),
            "{request:?}"
        );
        assert!(
            request
                .header("accept")
                .is_some_and(|accept| accept.contains("application/json")
                    && accept.contains("text/event-stream")),
            "{request:?}"
        );
    }
    let call = server.call("tools/call").expect("a tools/call request");
    assert_eq!(call.header("mcp-name"), Some("echo"));
    client.stop().await;
}

#[tokio::test]
async fn http_sse_reply() {
    let server = Server::start(Settings {
        sse: true,
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let cancel = CancellationToken::new();
    let output = client
        .call("echo", "{}", &cancel)
        .await
        .expect("calls echo");
    assert_eq!(output.text, "the echo tool ran");
    assert!(
        server.call("tools/call").is_some(),
        "no call reached the server"
    );
    client.stop().await;
}

#[tokio::test]
async fn http_retry_503() {
    let server = Server::start(Settings {
        fail: Some(("tools/call".to_owned(), 503)),
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let _ = client.tools().await.expect("lists tools");
    let cancel = CancellationToken::new();
    let output = client
        .call("echo", "{}", &cancel)
        .await
        .expect("calls echo after a 503");
    assert_eq!(output.text, "the echo tool ran");
    assert_eq!(
        server.calls("tools/call").len(),
        2,
        "the call was not retried"
    );
    client.stop().await;
}

#[tokio::test]
async fn http_header_from_env() {
    let expanded = expand_env("Bearer ${TOKEN}", |name| {
        (name == "TOKEN").then(|| "abc".to_owned())
    })
    .expect("expands the variable");
    assert_eq!(expanded, "Bearer abc");

    let missing = expand_env("Bearer ${NO_SUCH_VARIABLE}", |_| None)
        .expect_err("a missing variable is an error");
    assert!(
        missing.to_string().contains("NO_SUCH_VARIABLE"),
        "{missing}"
    );

    let server = Server::start(Settings {
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = connect(&server, vec![("Authorization".to_owned(), expanded)]).await;
    let _ = client.tools().await.expect("lists tools");
    let first = server.requests().into_iter().next().expect("a request");
    assert_eq!(first.header("authorization"), Some("Bearer abc"));
    client.stop().await;
}

#[tokio::test]
async fn http_legacy_session() {
    let server = Server::start(Settings {
        legacy: true,
        session: Some("s1".to_owned()),
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    assert_eq!(client.era(), Era::Legacy("2025-11-25"));
    let _ = client.tools().await.expect("lists tools");

    let initialize = server.call("initialize").expect("an initialize request");
    assert_eq!(initialize.header("mcp-session-id"), None, "{initialize:?}");
    assert_eq!(
        initialize.header("mcp-protocol-version"),
        None,
        "initialize names no negotiated version: {initialize:?}"
    );

    let list = server.call("tools/list").expect("a tools/list request");
    assert_eq!(list.header("mcp-session-id"), Some("s1"));
    assert_eq!(list.header("mcp-protocol-version"), Some("2025-11-25"));

    client.stop().await;
    let deleted = server.http("DELETE");
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0].header("mcp-session-id"), Some("s1"));
}

#[tokio::test]
async fn http_unauthorized() {
    let server = Server::start(Settings {
        unauthorized: Some(
            "Bearer resource_metadata=\"http://x/.well-known/oauth-protected-resource\"".to_owned(),
        ),
        ..Settings::default()
    })
    .await;
    let spec = HttpSpec {
        url: server.url.clone(),
        ..HttpSpec::default()
    };
    let error = Client::http(&spec).await.expect_err("a 401");
    match error {
        Error::Unauthorized { resource_metadata } => assert_eq!(
            resource_metadata.as_deref(),
            Some("http://x/.well-known/oauth-protected-resource")
        ),
        other => panic!("not an unauthorized error: {other:?}"),
    }
}

#[tokio::test]
async fn http_mcp_name_base64() {
    let server = Server::start(Settings {
        tools: vec![tool("h\u{e9}llo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let tools = client.tools().await.expect("lists tools");
    assert_eq!(tools[0].name, "h\u{e9}llo");

    let cancel = CancellationToken::new();
    client
        .call("h\u{e9}llo", "{}", &cancel)
        .await
        .expect("calls h\u{e9}llo");
    let call = server.call("tools/call").expect("a call");
    assert_eq!(call.header("mcp-name"), Some("=?base64?aMOpbGxv?="));
    client.stop().await;
}

#[tokio::test]
async fn http_x_mcp_header() {
    let region = json!({
        "type": "object",
        "properties": {"region": {"type": "string", "x-mcp-header": "Region"}},
    });
    let counted = json!({
        "type": "object",
        "properties": {"count": {"type": "number", "x-mcp-header": "Count"}},
    });
    let server = Server::start(Settings {
        tools: vec![tool("zone", region), tool("counted", counted)],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let names: Vec<String> = client
        .tools()
        .await
        .expect("lists tools")
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert_eq!(names, ["zone"], "the number header tool is dropped");

    let cancel = CancellationToken::new();
    client
        .call("zone", r#"{"region":"us-west1"}"#, &cancel)
        .await
        .expect("calls zone");
    let call = server.call("tools/call").expect("a call");
    assert_eq!(call.header("mcp-param-region"), Some("us-west1"));
    assert!(
        client
            .warnings()
            .iter()
            .any(|warning| warning.contains("counted")),
        "no warning about the dropped tool: {:?}",
        client.warnings()
    );
    client.stop().await;
}

#[tokio::test]
async fn http_header_mismatch_relists_and_retries() {
    let region = json!({
        "type": "object",
        "properties": {"region": {"type": "string", "x-mcp-header": "Region"}},
    });
    let server = Server::start(Settings {
        mismatch_first: true,
        tools: vec![tool("zone", region)],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let _ = client.tools().await.expect("lists tools");

    let cancel = CancellationToken::new();
    let output = client
        .call("zone", r#"{"region":"us-west1"}"#, &cancel)
        .await
        .expect("retries the call after a header mismatch");
    assert_eq!(output.text, "the zone tool ran");
    assert_eq!(
        server.calls("tools/call").len(),
        2,
        "the call was not retried"
    );
    assert_eq!(
        server.calls("tools/list").len(),
        2,
        "the tools were not listed again"
    );
    let call = server.call("tools/call").expect("a call");
    assert_eq!(call.header("mcp-param-region"), Some("us-west1"));
    assert_eq!(call.header("mcp-name"), Some("zone"));
    client.stop().await;
}

#[tokio::test]
async fn http_session_end_starts_a_new_one() {
    let server = Server::start(Settings {
        legacy: true,
        session: Some("s".to_owned()),
        expire_session: true,
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let tools = client
        .tools()
        .await
        .expect("lists tools after the session ended");
    assert_eq!(tools.len(), 1);
    assert_eq!(
        server.calls("initialize").len(),
        2,
        "the session was not started again"
    );
    let list = server.call("tools/list").expect("a tools/list request");
    assert_eq!(list.header("mcp-session-id"), Some("s2"));
    client.stop().await;
    let deleted = server.http("DELETE");
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0].header("mcp-session-id"), Some("s2"));
}

#[tokio::test]
async fn http_cancel_closes_stream() {
    let server = Server::start(Settings {
        sse: true,
        slow_ms: Some(5_000),
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let cancel = CancellationToken::new();
    let watcher = cancel.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(100)).await;
        watcher.cancel();
    });

    let start = Instant::now();
    let result = client.call("echo", "{}", &cancel).await;
    let elapsed = start.elapsed();
    let error = result.expect_err("a cancelled call");
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[tokio::test]
async fn http_server_ping_is_answered() {
    let server = Server::start(Settings {
        sse: true,
        ping_before_call: true,
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let cancel = CancellationToken::new();
    let output = client
        .call("echo", "{}", &cancel)
        .await
        .expect("calls echo");
    assert_eq!(output.text, "the echo tool ran");

    let answer = server
        .requests()
        .into_iter()
        .find(|request| {
            request
                .message()
                .is_some_and(|message| message.get("result").is_some())
        })
        .expect("an answer to the ping");
    let message = answer.message().expect("a message");
    assert_eq!(message["id"], json!(9001));
    assert_eq!(message["result"], json!({}));
    assert_eq!(answer.header("mcp-method"), Some("ping"));
    assert_eq!(answer.header("mcp-name"), None);
    client.stop().await;
}

#[tokio::test]
async fn http_old_sse_transport_fails() {
    let server = Server::start(Settings {
        old_sse: true,
        tools: Vec::new(),
        ..Settings::default()
    })
    .await;
    let spec = HttpSpec {
        url: server.url.clone(),
        ..HttpSpec::default()
    };
    let error = Client::http(&spec)
        .await
        .expect_err("an unsupported transport");
    assert!(error.to_string().contains("2024-11-05"), "{error}");
}

#[tokio::test]
async fn http_legacy_reinitialize_sends_initialized() {
    let server = Server::start(Settings {
        legacy: true,
        session: Some("s".to_owned()),
        expire_session: true,
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let _ = client
        .tools()
        .await
        .expect("lists tools after the session ended");

    let rpcs: Vec<String> = server
        .requests()
        .into_iter()
        .filter_map(|request| request.rpc())
        .collect();
    assert_eq!(
        rpcs,
        [
            "server/discover",
            "initialize",
            "notifications/initialized",
            "tools/list",
            "initialize",
            "notifications/initialized",
            "tools/list",
        ],
        "the second handshake did not end with the notification"
    );
    client.stop().await;
}

#[tokio::test]
async fn http_error_names_no_path_or_query() {
    let spec = HttpSpec {
        url: "http://127.0.0.1:1/secret-path?key=abc".to_owned(),
        ..HttpSpec::default()
    };
    let error = Client::http(&spec)
        .await
        .expect_err("nothing listens there")
        .to_string();
    assert!(!error.contains("secret-path"), "{error}");
    assert!(!error.contains("abc"), "{error}");
}

#[tokio::test]
async fn http_redirect_same_origin_followed() {
    let server = Server::start(Settings {
        redirect: Some((307, "/v2/mcp".to_owned())),
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let cancel = CancellationToken::new();
    let output = client
        .call("echo", "{}", &cancel)
        .await
        .expect("calls echo past the redirect");
    assert_eq!(output.text, "the echo tool ran");

    let call = server.call("tools/call").expect("a tools/call request");
    assert_eq!(call.path, "/v2/mcp");
    assert!(server.paths().iter().any(|path| path == "/mcp"));
    client.stop().await;
}

#[tokio::test]
async fn http_redirect_other_origin_fails() {
    let server = Server::start(Settings {
        redirect: Some((307, format!("http://127.0.0.2:{PORT}/x?key=abc"))),
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let spec = HttpSpec {
        url: server.url.clone(),
        ..HttpSpec::default()
    };
    let error = Client::http(&spec)
        .await
        .expect_err("another origin is not followed")
        .to_string();
    assert!(error.contains("127.0.0.2"), "{error}");
    assert!(error.contains("307"), "{error}");
    assert!(!error.contains("/x"), "{error}");
    assert!(!error.contains("abc"), "{error}");
}

#[tokio::test]
async fn http_stop_is_bounded() {
    let server = Server::start(Settings {
        legacy: true,
        session: Some("s1".to_owned()),
        silent_delete: true,
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let _ = client.tools().await.expect("lists tools");
    let start = Instant::now();
    client.stop().await;
    let elapsed = start.elapsed();
    assert_eq!(server.http("DELETE").len(), 1, "no session was ended");
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
}

#[tokio::test]
async fn http_body_past_the_cap_fails_the_request() {
    let server = Server::start(Settings {
        huge_body_mib: Some(HUGE_MIB),
        tools: vec![tool("echo", object())],
        ..Settings::default()
    })
    .await;
    let client = open(&server).await;
    let error = client.tools().await.expect_err("the body is too large");
    assert!(error.to_string().contains("16 MiB"), "{error}");
    client.stop().await;
}
