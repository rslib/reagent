//! End-to-end tests for `--provider`, `--base-url`, `--api-key-env` and
//! `--header`.
//!
//! Each test points the binary at a local TCP server that records the request
//! line and headers and answers with a short canned SSE reply. No real API is
//! called, and no key value is ever printed.

use std::process::Output;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::oneshot;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// One HTTP request the test server recorded.
#[derive(Debug)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
}

impl Recorded {
    /// The value of the header named `name`, or `None`.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Serves one request with `reply` as its SSE body and records it.
async fn serve(reply: &'static str) -> (String, oneshot::Receiver<Recorded>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
    let address = listener.local_addr().expect("the bound address");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept a connection");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&buffer);
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_owned();
        let headers = lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(": "))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
            .collect();
        let _ = sender.send(Recorded {
            request_line,
            headers,
        });
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            reply.len(),
            reply
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{address}"), receiver)
}

const ANTHROPIC_REPLY: &str = concat!(
    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);
const CHAT_REPLY: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);
const RESPONSES_REPLY: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"output_index\":0,\"delta\":\"ok\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
);

/// Runs the binary with `args`, `env` set and both default key vars removed.
async fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BIN);
    command.args(args);
    // The user's real config must not change the test.
    command.env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config");
    command.env_remove("ANTHROPIC_API_KEY");
    command.env_remove("OPENAI_API_KEY");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().await.expect("the binary runs")
}

/// Asserts the run succeeded and printed the reply text.
fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
}

/// Asserts the key value is in neither output stream.
fn assert_key_hidden(output: &Output, key: &str) {
    assert!(!String::from_utf8_lossy(&output.stdout).contains(key));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(key));
}

#[tokio::test]
async fn provider_select_anthropic_posts_to_messages() {
    let (base, recorded) = serve(ANTHROPIC_REPLY).await;
    let output = run(
        &[
            "--provider",
            "anthropic",
            "--base-url",
            base.as_str(),
            "--header",
            "x-extra:yes",
            "-p",
            "hi",
        ],
        &[("ANTHROPIC_API_KEY", "sk-ant-secret")],
    )
    .await;
    assert_ok(&output);
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "POST /v1/messages HTTP/1.1");
    assert_eq!(recorded.header("x-api-key"), Some("sk-ant-secret"));
    assert_eq!(recorded.header("x-extra"), Some("yes"));
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert_key_hidden(&output, "sk-ant-secret");
}

#[tokio::test]
async fn provider_select_openai_chat_posts_to_chat_completions() {
    let (base, recorded) = serve(CHAT_REPLY).await;
    let output = run(
        &[
            "--provider",
            "openai-chat",
            "--base-url",
            base.as_str(),
            "--header",
            "x-extra:yes",
            "-p",
            "hi",
        ],
        &[("OPENAI_API_KEY", "sk-openai-secret")],
    )
    .await;
    assert_ok(&output);
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "POST /chat/completions HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        Some("Bearer sk-openai-secret")
    );
    assert_eq!(recorded.header("x-extra"), Some("yes"));
    assert_key_hidden(&output, "sk-openai-secret");
}

#[tokio::test]
async fn provider_select_openai_responses_posts_to_responses() {
    let (base, recorded) = serve(RESPONSES_REPLY).await;
    let output = run(
        &[
            "--provider",
            "openai-responses",
            "--base-url",
            base.as_str(),
            "--header",
            "x-extra:yes",
            "-p",
            "hi",
        ],
        &[("OPENAI_API_KEY", "sk-openai-secret")],
    )
    .await;
    assert_ok(&output);
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "POST /responses HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        Some("Bearer sk-openai-secret")
    );
    assert_eq!(recorded.header("x-extra"), Some("yes"));
    assert_key_hidden(&output, "sk-openai-secret");
}

#[tokio::test]
async fn provider_select_the_key_env_can_be_chosen() {
    let (base, recorded) = serve(ANTHROPIC_REPLY).await;
    let output = run(
        &[
            "--provider",
            "anthropic",
            "--base-url",
            base.as_str(),
            "--api-key-env",
            "MY_GATEWAY_KEY",
            "-p",
            "hi",
        ],
        &[("MY_GATEWAY_KEY", "gateway-secret")],
    )
    .await;
    assert_ok(&output);
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.header("x-api-key"), Some("gateway-secret"));
    assert_key_hidden(&output, "gateway-secret");
}

#[tokio::test]
async fn provider_select_a_missing_key_exits_2_with_the_variable_name() {
    let output = run(
        &[
            "--provider",
            "openai-responses",
            "--base-url",
            "http://127.0.0.1:1",
            "-p",
            "hi",
        ],
        &[],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "reagent: missing OPENAI_API_KEY\n"
    );
}

#[tokio::test]
async fn provider_select_a_named_missing_key_exits_2_with_its_name() {
    let output = run(
        &[
            "--provider",
            "anthropic",
            "--base-url",
            "http://127.0.0.1:1",
            "--api-key-env",
            "MY_KEY",
            "-p",
            "hi",
        ],
        &[],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "reagent: missing MY_KEY\n"
    );
}

#[tokio::test]
async fn provider_select_an_empty_key_var_exits_2() {
    let output = run(
        &[
            "--provider",
            "openai-chat",
            "--base-url",
            "http://127.0.0.1:1",
            "-p",
            "hi",
        ],
        &[("OPENAI_API_KEY", "")],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "reagent: missing OPENAI_API_KEY\n"
    );
}
