//! End-to-end tests for the built-in provider presets.
//!
//! Each test writes a config in a temp home, points a provider table at a
//! local TCP server that records the request and answers with a canned reply.
//! No real API is called, and the fake keys are never printed.

use std::os::unix::fs::PermissionsExt;
use std::process::Output;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::oneshot;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
const KEY: &str = "sk-preset-fake";
const CHAT_REPLY: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);
const LIST: &str = r#"{"data":[{"id":"b"},{"id":"a"}]}"#;
const PI_REPLY: &str = concat!(
    "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"ok\"}\n\n",
    "data: {\"type\":\"text_end\",\"contentIndex\":0}\n\n",
    "data: {\"type\":\"done\",\"reason\":\"stop\"}\n\n"
);
/// The endpoint's own model list, as `/config` answers it.
const RADIUS_CONFIG: &str = r#"{"baseUrl":"https://radius.pi.dev/v1","models":[
    {"id":"balanced","name":"Balanced","contextWindow":200000,"maxTokens":64000,
     "cost":{"input":1.0,"output":4.0,"cacheRead":0.1,"cacheWrite":1.0},"enabled":true},
    {"id":"small","name":"Small","contextWindow":32000,"maxTokens":8000,"enabled":true},
    {"id":"retired","name":"Retired","contextWindow":8000,"maxTokens":1000,"enabled":false}
]}"#;

/// The HTTP request the test server recorded.
#[derive(Debug)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Serves one request with `reply` and records it, body included.
async fn serve(
    content_type: &'static str,
    reply: &'static str,
) -> (String, oneshot::Receiver<Recorded>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
    let address = listener.local_addr().expect("the bound address");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept a connection");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        let (head_end, length) = loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            buffer.extend_from_slice(&chunk[..read]);
            let end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
            if let Some(end) = end {
                let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (end + 4, length);
            }
            if read == 0 {
                break (buffer.len(), 0);
            }
        };
        while buffer.len() < head_end + length {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let head = String::from_utf8_lossy(&buffer[..head_end.saturating_sub(4)]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(": "))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
            .collect();
        let body = String::from_utf8_lossy(&buffer[head_end.min(buffer.len())..]).into_owned();
        let _ = sender.send(Recorded {
            request_line,
            headers,
            body,
        });
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{address}/v1"), receiver)
}

/// Runs the binary in a temp home that holds `config`.
async fn run(config: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let dir = TempDir::new().expect("a temp dir");
    let root = dir.path();
    let folder = root.join("config/reagent");
    tokio::fs::create_dir_all(&folder)
        .await
        .expect("config dir");
    let file = folder.join("config.toml");
    tokio::fs::write(&file, config)
        .await
        .expect("write the config");
    tokio::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .await
        .expect("restrict the config");
    let mut command = Command::new(BIN);
    command.args(args);
    command.current_dir(root);
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_CACHE_HOME", root.join("cache"));
    command.env("HOME", root);
    // A closed port, so a models listing never reaches the real models.dev.
    command.env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:9/api.json");
    for name in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "OPENCODE_API_KEY",
        "COMMANDCODE_API_KEY",
        "FREEINFERENCE_API_KEY",
        "DEEPSEEK_API_KEY",
        "RADIUS_API_KEY",
        "RADIUS_BASE_URL",
    ] {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().await.expect("the binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test]
async fn preset_table_with_only_a_base_url_keeps_the_preset_key_and_wire() {
    let (base, recorded) = serve("text/event-stream", CHAT_REPLY).await;
    let config = format!("[providers.openrouter]\nbase_url = \"{base}\"\n");
    let output = run(
        &config,
        &["--model", "openrouter/x", "-p", "hi"],
        &[("OPENROUTER_API_KEY", KEY)],
    )
    .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "ok\n");
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(!stdout(&output).contains(KEY) && !stderr(&output).contains(KEY));
}

#[tokio::test]
async fn preset_compat_sets_max_completion_tokens_on_a_chat_table() {
    let (base, recorded) = serve("text/event-stream", CHAT_REPLY).await;
    let config = format!(
        "[providers.fi]\nwire = \"openai-chat\"\nbase_url = \"{base}\"\napi_key_env = \"FI_KEY\"\n\
         [providers.fi.compat]\nmax_tokens_field = \"max_completion_tokens\"\n"
    );
    let output = run(
        &config,
        &["--model", "fi/x", "-p", "hi"],
        &[("FI_KEY", KEY)],
    )
    .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    let recorded = recorded.await.expect("the server recorded the request");
    assert!(
        recorded.body.contains("\"max_completion_tokens\""),
        "{}",
        recorded.body
    );
    assert!(
        !recorded.body.contains("\"max_tokens\""),
        "{}",
        recorded.body
    );
}

#[tokio::test]
async fn preset_unknown_compat_flag_fails_at_start() {
    let config = "[providers.fi.compat]\nnope = true\n";
    let output = run(config, &["--model", "fi/x", "-p", "hi"], &[]).await;
    assert_eq!(output.status.code(), Some(2), "stderr {}", stderr(&output));
    assert!(
        stderr(&output).contains("config: [providers.fi.compat]: "),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn preset_models_without_a_name_lists_a_preset_with_a_key() {
    let (base, recorded) = serve("application/json", LIST).await;
    let config = format!("[providers.openrouter]\nbase_url = \"{base}\"\n");
    let output = run(&config, &["models"], &[("OPENROUTER_API_KEY", KEY)]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "openrouter/a\nopenrouter/b\n");
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "GET /v1/models HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
}

#[tokio::test]
async fn preset_models_without_a_name_skips_a_preset_with_no_key() {
    let output = run("", &["models"], &[]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "");
    let output = run("", &["models", "opencode-go"], &[]).await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "reagent: opencode-go: missing OPENCODE_API_KEY\n"
    );
}

#[tokio::test]
async fn preset_base_url_variable_still_wins_for_openai() {
    let (base, recorded) = serve("application/json", LIST).await;
    let output = run(
        "",
        &["models", "openai", "--refresh"],
        &[("OPENAI_API_KEY", KEY), ("OPENAI_BASE_URL", &base)],
    )
    .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "openai/a\nopenai/b\n");
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "GET /v1/models HTTP/1.1");
}

#[tokio::test]
async fn route_openrouter_messages_sends_a_bearer_key() {
    let (base, recorded) = serve("text/event-stream", CHAT_REPLY).await;
    let config = format!("[providers.openrouter]\nwire = \"anthropic\"\nbase_url = \"{base}\"\n");
    let output = run(
        &config,
        &[
            "--model",
            "openrouter/anthropic/claude-sonnet-5",
            "-p",
            "hi",
        ],
        &[("OPENROUTER_API_KEY", KEY)],
    )
    .await;
    // The reply is a chat stream, so only the request it sent matters here.
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "POST /v1/v1/messages HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(recorded.header("x-api-key"), None);
    assert!(!stdout(&output).contains(KEY) && !stderr(&output).contains(KEY));
}

#[tokio::test]
async fn radius_preset_streams_a_pi_messages_reply() {
    let (base, recorded) = serve("text/event-stream", PI_REPLY).await;
    let output = run(
        "",
        &["--model", "radius/balanced", "-p", "hi"],
        &[("RADIUS_API_KEY", KEY), ("RADIUS_BASE_URL", &base)],
    )
    .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "ok\n");
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "POST /v1/messages HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(recorded.header("accept"), Some("text/event-stream"));
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert!(
        recorded.body.contains("\"model\":\"balanced\""),
        "{}",
        recorded.body
    );
    assert!(
        recorded.body.contains("\"context\":{\"messages\":["),
        "{}",
        recorded.body
    );
    assert!(
        recorded
            .body
            .contains("\"role\":\"user\",\"content\":\"hi\\n\\n[environment]\\n"),
        "{}",
        recorded.body
    );
    assert!(!stdout(&output).contains(KEY) && !stderr(&output).contains(KEY));
}

#[tokio::test]
async fn radius_models_lists_the_public_config() {
    let (base, recorded) = serve("application/json", RADIUS_CONFIG).await;
    let output = run("", &["models", "radius"], &[("RADIUS_BASE_URL", &base)]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "radius/balanced\nradius/small\n");
    let recorded = recorded.await.expect("the server recorded the request");
    assert_eq!(recorded.request_line, "GET /v1/config HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        None,
        "a public list needs no key"
    );
}

/// The public list of the radius endpoint is what a run with no credential
/// lists: a key the endpoint would refuse, or an auth file this build cannot
/// read, is an error and not a reason to list the models without one.
#[tokio::test]
async fn radius_models_reports_a_credential_error_instead_of_the_public_list() {
    // A key with a newline is not a header value, so the credential fails.
    let (base, _recorded) = serve("application/json", RADIUS_CONFIG).await;
    let output = run(
        "",
        &["models", "radius"],
        &[("RADIUS_BASE_URL", &base), ("RADIUS_API_KEY", "bad\nkey")],
    )
    .await;
    assert_eq!(output.status.code(), Some(1), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "", "no list without a credential");
    assert!(
        stderr(&output).contains("the API key is not a valid authorization header value"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("listing the public models"),
        "{}",
        stderr(&output)
    );

    // An auth file this build refuses is an error too.
    let state = TempDir::new().expect("a temp dir");
    let folder = state.path().join("reagent");
    tokio::fs::create_dir_all(&folder).await.expect("auth dir");
    let file = folder.join("auth.json");
    tokio::fs::write(&file, "{\"radius\":{\"key\":\"sk-fake\"}}")
        .await
        .expect("write the auth file");
    tokio::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
        .await
        .expect("widen the auth file");
    let state = state.path().to_str().expect("a UTF-8 path").to_owned();
    let (base, _recorded) = serve("application/json", RADIUS_CONFIG).await;
    let output = run(
        "",
        &["models", "radius"],
        &[("RADIUS_BASE_URL", &base), ("XDG_STATE_HOME", &state)],
    )
    .await;
    assert_eq!(output.status.code(), Some(1), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "", "no list from a broken auth file");
    assert!(stderr(&output).contains("auth.json"), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("listing the public models"),
        "{}",
        stderr(&output)
    );
}
