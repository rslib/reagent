//! Era detection: the modern probe, the legacy handshake and its version check.

mod common;

use common::Fake;
use reagent_mcp::{Client, Era};
use serde_json::json;

#[tokio::test]
async fn version_modern_server_gets_no_initialize() {
    let fake = Fake::new(json!({"era": "modern"}));
    let client = Client::stdio(&fake.spec).await.expect("connects");
    assert_eq!(client.era(), Era::Modern("2026-07-28"));
    let tools = client.tools().await.expect("lists tools");
    assert!(!tools.is_empty(), "no tools");

    let messages = fake.messages().await;
    let methods: Vec<&str> = messages
        .iter()
        .filter_map(|message| message["method"].as_str())
        .collect();
    assert_eq!(methods, ["server/discover", "tools/list"]);

    let list = fake.method("tools/list").await;
    let meta = &list["params"]["_meta"];
    assert_eq!(
        meta["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(
        meta["io.modelcontextprotocol/clientInfo"]["name"],
        "reagent"
    );
    assert!(
        meta["io.modelcontextprotocol/clientCapabilities"].is_object(),
        "{meta}"
    );
    client.stop().await;
}

#[tokio::test]
async fn version_legacy_server_gets_the_handshake() {
    let fake = Fake::new(json!({"era": "legacy", "version": "2025-06-18"}));
    let client = Client::stdio(&fake.spec).await.expect("connects");
    assert_eq!(client.era(), Era::Legacy("2025-06-18"));
    assert!(!client.tools().await.expect("lists tools").is_empty());

    assert_eq!(
        fake.methods().await,
        [
            "server/discover",
            "initialize",
            "notifications/initialized",
            "tools/list"
        ]
    );
    client.stop().await;
}

#[tokio::test]
async fn version_unknown_legacy_version_fails() {
    let fake = Fake::new(json!({"era": "legacy", "version": "2024-11-05"}));
    let error = Client::stdio(&fake.spec)
        .await
        .expect_err("an error for an unknown version");
    let text = error.to_string();
    assert!(text.contains("2024-11-05"), "{text}");
    assert!(text.contains("2025-11-25"), "{text}");
}

#[tokio::test]
async fn version_discover_needs_a_shared_version() {
    let unsupported = Fake::new(json!({"discover_supported": ["2025-08-01"]}));
    let error = Client::stdio(&unsupported.spec)
        .await
        .expect_err("no shared version");
    let text = error.to_string();
    assert!(text.contains("2025-08-01"), "{text}");
    assert!(text.contains("2026-07-28"), "{text}");

    let shared = Fake::new(json!({"discover_supported": ["2026-07-28"]}));
    let client = Client::stdio(&shared.spec).await.expect("connects");
    assert_eq!(client.era(), Era::Modern("2026-07-28"));
    assert!(!client.tools().await.expect("lists tools").is_empty());
    client.stop().await;
}
