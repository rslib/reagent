//! End-to-end tests for `reagent models`.
//!
//! Each test points a provider table at a local TCP server that records the
//! requests and answers with a canned model list. No real API is called, and
//! the fake key is never printed.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
const KEY: &str = "sk-models-secret";
const LIST: &str = r#"{"data":[{"id":"b","context_length":8000},{"id":"a"}]}"#;

/// One HTTP request the test server recorded.
#[derive(Debug)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Answers every request with `status` and `body`, and records each one.
async fn serve(
    status: &'static str,
    body: &'static str,
) -> (String, mpsc::UnboundedReceiver<Recorded>) {
    serve_with(status, "", body).await
}

/// Like [`serve`], with `extra` header lines in each reply.
async fn serve_with(
    status: &'static str,
    extra: &str,
    body: &'static str,
) -> (String, mpsc::UnboundedReceiver<Recorded>) {
    let extra = extra.to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
    let address = listener.local_addr().expect("the bound address");
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let sender = sender.clone();
            let extra = extra.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
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
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{address}/v1"), receiver)
}

/// A temp home with a config for provider `fi` and room for the cache.
struct Home {
    dir: TempDir,
    /// A catalogue URL on a port nothing listens on, so a fill fails at once
    /// and no test reaches the real models.dev.
    catalogue: String,
}

impl Home {
    async fn new(base: &str) -> Self {
        Self::with_config(&format!(
            "[providers.fi]\nwire = \"openai-chat\"\nbase_url = \"{base}\"\napi_key_env = \"FI_MODELS_KEY\"\n"
        ))
        .await
    }

    async fn with_config(text: &str) -> Self {
        let dir = TempDir::new().expect("a temp dir");
        let config = dir.path().join("config/reagent");
        tokio::fs::create_dir_all(&config)
            .await
            .expect("config dir");
        let file = config.join("config.toml");
        tokio::fs::write(&file, text)
            .await
            .expect("write the config");
        tokio::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .await
            .expect("restrict the config");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let dead = listener.local_addr().expect("the bound address");
        drop(listener);
        Self {
            dir,
            catalogue: format!("http://{dead}/api.json"),
        }
    }

    fn cache_file(&self) -> std::path::PathBuf {
        self.dir.path().join("cache/reagent/models/fi.json")
    }

    async fn write_cache(&self, fetched_at: u64, ids: &[&str]) {
        let models: Vec<String> = ids
            .iter()
            .map(|id| format!("{{\"id\":\"{id}\",\"context_window\":null,\"max_output\":null}}"))
            .collect();
        let text = format!(
            "{{\"fetched_at\":{fetched_at},\"models\":[{}]}}",
            models.join(",")
        );
        let file = self.cache_file();
        tokio::fs::create_dir_all(file.parent().expect("a parent"))
            .await
            .expect("cache dir");
        tokio::fs::write(&file, text)
            .await
            .expect("write the cache");
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[]).await
    }

    async fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let root: &Path = self.dir.path();
        let mut command = Command::new(BIN);
        command.args(args);
        command.current_dir(root);
        command.env("XDG_CONFIG_HOME", root.join("config"));
        command.env("XDG_CACHE_HOME", root.join("cache"));
        command.env("HOME", root);
        command.env("FI_MODELS_KEY", KEY);
        // Tests never reach the real catalogue; a test that serves its own
        // reply overrides this.
        command.env("REAGENT_MODELS_DEV_URL", &self.catalogue);
        command.env_remove("REAGENT_PROFILE");
        command.env_remove("ANTHROPIC_API_KEY");
        command.env_remove("OPENAI_API_KEY");
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().await.expect("the binary runs")
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A temp home whose config names `codex-sub` on `base`, with a saved ChatGPT
/// login for it.
async fn home_with_a_saved_login(base: &str) -> Home {
    let home = Home::with_config(&format!(
        "[providers.codex-sub]\npreset = \"chatgpt\"\nbase_url = \"{base}\"\n"
    ))
    .await;
    let state = home.dir.path().join("state/reagent");
    tokio::fs::create_dir_all(&state).await.expect("state dir");
    let auth = state.join("auth.json");
    let far = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .as_secs()
        + 3600;
    tokio::fs::write(
        &auth,
        format!(
            r#"{{"codex-sub":{{"type":"oauth","client_id":"oaiapp_test","host_id":"urn:uuid:00000000-0000-4000-8000-000000000000","access":"at-models-test","refresh":"rt-models-test","id_token":"id-test","expires_at":{far},"subject":"sub","email":null}}}}"#
        ),
    )
    .await
    .expect("write the login");
    tokio::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600))
        .await
        .expect("restrict the login");
    home
}

#[tokio::test]
async fn models_lists_sorted_ids_and_writes_the_cache() {
    let (base, mut requests) = serve("200 OK", LIST).await;
    let home = Home::new(&base).await;
    let output = home.run(&["models"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "fi/a\nfi/b\n");
    let recorded = requests.recv().await.expect("one request");
    assert_eq!(recorded.request_line, "GET /v1/models HTTP/1.1");
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    let cache = tokio::fs::read_to_string(home.cache_file())
        .await
        .expect("the cache exists");
    assert!(cache.contains("\"fetched_at\""), "{cache}");
    assert!(cache.contains("\"context_window\":8000"), "{cache}");
    assert!(!cache.contains(KEY));
    assert!(!stderr(&output).contains(KEY));
}

#[tokio::test]
async fn models_uses_a_fresh_cache_without_a_request() {
    let (base, mut requests) = serve("500 Internal Server Error", "{}").await;
    let home = Home::new(&base).await;
    home.write_cache(now() - 60, &["z", "y"]).await;
    let output = home.run(&["models", "fi"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "fi/y\nfi/z\n");
    assert!(requests.try_recv().is_err(), "no request is expected");
}

#[tokio::test]
async fn models_refresh_fetches_despite_a_fresh_cache() {
    let (base, mut requests) = serve("200 OK", LIST).await;
    let home = Home::new(&base).await;
    home.write_cache(now() - 60, &["z"]).await;
    let output = home.run(&["models", "--refresh"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "fi/a\nfi/b\n");
    assert!(requests.recv().await.is_some());
}

#[tokio::test]
async fn models_fetches_when_the_cache_is_stale() {
    let (base, mut requests) = serve("200 OK", LIST).await;
    let home = Home::new(&base).await;
    home.write_cache(now() - 3 * 24 * 60 * 60, &["z"]).await;
    let output = home.run(&["models"]).await;
    assert_eq!(stdout(&output), "fi/a\nfi/b\n");
    assert!(requests.recv().await.is_some());
}

#[tokio::test]
async fn models_a_failing_server_without_a_cache_exits_1() {
    let (base, _requests) = serve("401 Unauthorized", "{}").await;
    let home = Home::new(&base).await;
    let output = home.run(&["models"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    let error = stderr(&output);
    // The start line comes first, so the error is one of the later lines.
    assert!(
        error.lines().any(|line| line.starts_with("reagent: fi: ")),
        "{error}"
    );
    assert!(error.contains("401"), "{error}");
    assert!(!error.contains(KEY), "{error}");
}

#[tokio::test]
async fn models_a_failed_refresh_falls_back_to_the_stale_cache() {
    let (base, _requests) = serve("500 Internal Server Error", "{}").await;
    let home = Home::new(&base).await;
    home.write_cache(now() - 3 * 24 * 60 * 60, &["z"]).await;
    let output = home.run(&["models"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "fi/z\n");
    let error = stderr(&output);
    // The start line comes first, so the error is one of the later lines.
    assert!(
        error.lines().any(|line| line.starts_with("reagent: fi: ")),
        "{error}"
    );
    assert!(error.contains("using the cached list from"), "{error}");
    assert!(!error.contains(KEY), "{error}");
}

#[tokio::test]
async fn models_a_missing_key_names_the_variable() {
    let (base, _requests) = serve("200 OK", LIST).await;
    let home = Home::new(&base).await;
    let root = home.dir.path();
    let output = Command::new(BIN)
        .args(["models", "fi"])
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("HOME", root)
        .env_remove("REAGENT_PROFILE")
        .env_remove("FI_MODELS_KEY")
        .output()
        .await
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "reagent: fi: missing FI_MODELS_KEY\n");
}

#[tokio::test]
async fn models_a_named_built_in_without_a_key_fails() {
    let home = Home::with_config("").await;
    let output = home.run(&["models", "anthropic"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "reagent: anthropic: missing ANTHROPIC_API_KEY\n"
    );
    let output = home.run(&["models"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "");
}

#[tokio::test]
async fn models_one_failing_provider_does_not_stop_the_others() {
    let (good, _good_requests) = serve("200 OK", LIST).await;
    let (bad, _bad_requests) = serve("500 Internal Server Error", "{}").await;
    let home = Home::with_config(&format!(
        "[providers.fi]\nwire = \"openai-chat\"\nbase_url = \"{good}\"\napi_key_env = \"FI_MODELS_KEY\"\n\
         [providers.zz]\nwire = \"openai-chat\"\nbase_url = \"{bad}\"\napi_key_env = \"FI_MODELS_KEY\"\n"
    ))
    .await;
    let output = home.run(&["models"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "fi/a\nfi/b\n");
    let error = stderr(&output);
    assert!(
        error.lines().any(|line| line.starts_with("reagent: zz: ")),
        "{error}"
    );
    assert!(error.contains("500"), "{error}");
}

#[tokio::test]
async fn models_anthropic_wire_sends_x_api_key() {
    let (base, mut requests) = serve("200 OK", LIST).await;
    let home = Home::with_config(&format!(
        "[providers.fi]\nwire = \"anthropic\"\nbase_url = \"{base}\"\napi_key_env = \"FI_MODELS_KEY\"\n"
    ))
    .await;
    let output = home.run(&["models", "fi"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    let recorded = requests.recv().await.expect("one request");
    assert_eq!(recorded.header("x-api-key"), Some(KEY));
    assert_eq!(recorded.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(recorded.header("authorization"), None);
}

#[tokio::test]
async fn models_file_key_beats_the_wire_default_variable() {
    let (base, mut requests) = serve("200 OK", LIST).await;
    let home = Home::with_config(&format!(
        "[providers.fi]\nwire = \"openai-chat\"\nbase_url = \"{base}\"\napi_key = \"sk-file-test\"\n"
    ))
    .await;
    let output = home.run(&["models", "fi"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    let recorded = requests.recv().await.expect("one request");
    assert_eq!(
        recorded.header("authorization"),
        Some("Bearer sk-file-test")
    );

    home.run_env(
        &["models", "fi", "--refresh"],
        &[("OPENAI_API_KEY", "sk-wire-default")],
    )
    .await;
    let recorded = requests.recv().await.expect("a second request");
    assert_eq!(
        recorded.header("authorization"),
        Some("Bearer sk-file-test")
    );
}

#[tokio::test]
async fn models_wire_default_variable_is_the_last_resort() {
    let (base, mut requests) = serve("200 OK", LIST).await;
    let home = Home::with_config(&format!(
        "[providers.fi]\nwire = \"openai-chat\"\nbase_url = \"{base}\"\n"
    ))
    .await;
    let output = home
        .run_env(&["models", "fi"], &[("OPENAI_API_KEY", "sk-wire-default")])
        .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    let recorded = requests.recv().await.expect("one request");
    assert_eq!(
        recorded.header("authorization"),
        Some("Bearer sk-wire-default")
    );
}

#[tokio::test]
async fn models_explicit_key_variable_beats_the_file_key() {
    let (base, mut requests) = serve("200 OK", LIST).await;
    let home = Home::with_config(&format!(
        "[providers.fi]\nwire = \"openai-chat\"\nbase_url = \"{base}\"\napi_key_env = \"FI_MODELS_KEY\"\napi_key = \"sk-file-test\"\n"
    ))
    .await;
    let output = home.run(&["models", "fi"]).await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    let recorded = requests.recv().await.expect("one request");
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
}

#[tokio::test]
async fn models_a_redirect_is_an_error_and_is_not_followed() {
    let (target, mut target_requests) = serve("200 OK", LIST).await;
    let location = format!("Location: {target}/models\r\n");
    let (base, _requests) = serve_with("302 Found", &location, "").await;
    let home = Home::new(&base).await;
    let output = home.run(&["models"]).await;
    assert_eq!(output.status.code(), Some(1));
    let error = stderr(&output);
    // The start line comes first, so the error is one of the later lines.
    assert!(
        error.lines().any(|line| line.starts_with("reagent: fi: ")),
        "{error}"
    );
    assert!(error.contains("302"), "{error}");
    assert!(
        target_requests.try_recv().is_err(),
        "the redirect was followed"
    );
}

const PLAN_LIST: &str = r#"{"models":[{"slug":"gpt-x","display_name":"GPT X","visibility":"list"},{"slug":"gpt-hidden","visibility":"hide"}]}"#;

/// A trimmed catalogue reply for the deepseek provider.
const CATALOGUE: &str = r#"{"deepseek":{"id":"deepseek","name":"DeepSeek","models":{
    "deepseek-flash":{"id":"deepseek-flash","name":"DeepSeek V4.1 Flash",
        "limit":{"context":1048576,"output":393216},
        "cost":{"input":0.15,"output":0.6,"cache_read":0.003}},
    "deepseek-v4-pro":{"id":"deepseek-v4-pro",
        "limit":{"context":1048576,"output":393216},
        "cost":{"input":0.66,"output":1.98}}
}}}"#;

/// A trimmed catalogue reply for the openai provider, used by the plan test.
const OPENAI_CATALOGUE: &str = r#"{"openai":{"id":"openai","models":{
    "gpt-x":{"id":"gpt-x","limit":{"context":1050000,"output":128000},
        "cost":{"input":2,"output":10}}
}}}"#;

/// A provider table that resolves the deepseek preset, with a fake endpoint.
fn deepseek_config(base: &str) -> String {
    format!("[providers.deepseek]\nbase_url = \"{base}\"\napi_key_env = \"FI_MODELS_KEY\"\n")
}

#[tokio::test]
async fn models_dev_a_fetch_error_does_not_fail_the_command() {
    let (base, _requests) = serve("200 OK", LIST).await;
    let home = Home::with_config(&deepseek_config(&base)).await;
    // A port nothing listens on: the catalogue fetch cannot connect.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
    let address = listener.local_addr().expect("the bound address");
    drop(listener);
    let url = format!("http://{address}/api.json");
    let output = home
        .run_env(
            &["models", "deepseek"],
            &[("REAGENT_MODELS_DEV_URL", url.as_str())],
        )
        .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "deepseek/a\ndeepseek/b\n");
    let error = stderr(&output);
    assert!(
        error
            .lines()
            .any(|line| line.starts_with("reagent: models.dev: ")),
        "{error}"
    );
    // The list is still cached, without a price.
    let cache =
        tokio::fs::read_to_string(home.dir.path().join("cache/reagent/models/deepseek.json"))
            .await
            .expect("the cache exists");
    assert!(!cache.contains("price"), "{cache}");
    assert!(!error.contains(KEY), "{error}");
}

#[tokio::test]
async fn models_dev_a_catalogue_price_fills_the_provider_cache() {
    let (base, _requests) = serve("200 OK", r#"{"data":[{"id":"deepseek-flash"}]}"#).await;
    let (catalogue, _catalogue_requests) = serve("200 OK", CATALOGUE).await;
    let home = Home::with_config(&deepseek_config(&base)).await;
    let output = home
        .run_env(
            &["models", "deepseek"],
            &[("REAGENT_MODELS_DEV_URL", catalogue.as_str())],
        )
        .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    assert_eq!(stdout(&output), "deepseek/deepseek-flash\n");
    let text =
        tokio::fs::read_to_string(home.dir.path().join("cache/reagent/models/deepseek.json"))
            .await
            .expect("the cache exists");
    let cache: serde_json::Value = serde_json::from_str(&text).expect("the cache is JSON");
    let entry = &cache["models"][0];
    assert_eq!(entry["id"], "deepseek-flash");
    assert_eq!(entry["context_window"], 1_048_576);
    assert_eq!(entry["max_output"], 393_216);
    assert_eq!(entry["price_source"], "models_dev");
    let price = |part: &str| entry["price"][part].as_f64().expect("a price part");
    assert!((price("input") - 0.15e-6).abs() < 1e-15, "{text}");
    assert!((price("output") - 0.6e-6).abs() < 1e-15, "{text}");
    assert!((price("cache_read") - 0.003e-6).abs() < 1e-15, "{text}");
    // The catalogue cache holds only the provider asked for.
    let text =
        tokio::fs::read_to_string(home.dir.path().join("cache/reagent/models/models.dev.json"))
            .await
            .expect("the catalogue cache exists");
    assert!(text.contains("\"deepseek\""), "{text}");
    assert!(!text.contains("\"openai\""), "{text}");
}

#[tokio::test]
async fn models_dev_a_plan_fills_the_limits_but_no_price() {
    let (base, _requests) = serve("200 OK", PLAN_LIST).await;
    let (catalogue, _catalogue_requests) = serve("200 OK", OPENAI_CATALOGUE).await;
    let home = home_with_a_saved_login(&base).await;
    let state_home = home.dir.path().join("state");
    let output = home
        .run_env(
            &["models", "codex-sub", "--refresh"],
            &[
                ("XDG_STATE_HOME", state_home.to_str().expect("utf-8 path")),
                ("REAGENT_MODELS_DEV_URL", catalogue.as_str()),
            ],
        )
        .await;
    assert!(output.status.success(), "stderr {}", stderr(&output));
    let text =
        tokio::fs::read_to_string(home.dir.path().join("cache/reagent/models/codex-sub.json"))
            .await
            .expect("the cache exists");
    let cache: serde_json::Value = serde_json::from_str(&text).expect("the cache is JSON");
    let entry = &cache["models"][0];
    assert_eq!(entry["id"], "gpt-x");
    assert_eq!(entry["context_window"], 1_050_000);
    assert_eq!(entry["max_output"], 128_000);
    assert_eq!(entry["price"], serde_json::Value::Null, "{text}");
    assert_eq!(entry["price_source"], serde_json::Value::Null, "{text}");
}

#[tokio::test]
async fn named_account_models_sends_the_saved_login_bearer() {
    let (base, mut requests) = serve("200 OK", PLAN_LIST).await;
    let home = home_with_a_saved_login(&base).await;
    let state_home = home.dir.path().join("state");
    let output = home
        .run_env(
            &["models", "codex-sub", "--refresh"],
            &[("XDG_STATE_HOME", state_home.to_str().expect("utf-8 path"))],
        )
        .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("gpt-x"), "{stdout}");
    assert!(!stdout.contains("gpt-hidden"), "{stdout}");
    let request = requests.recv().await.expect("one request");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer at-models-test")
    );
    assert!(!stdout.contains("at-models-test"));
}

#[tokio::test]
async fn start_line_the_models_line_names_the_saved_login() {
    let (base, mut requests) = serve("200 OK", PLAN_LIST).await;
    let home = home_with_a_saved_login(&base).await;
    let state_home = home.dir.path().join("state");
    let output = home
        .run_env(
            &["models", "codex-sub", "--refresh"],
            &[("XDG_STATE_HOME", state_home.to_str().expect("utf-8 path"))],
        )
        .await;
    let error = stderr(&output);
    assert!(output.status.success(), "{error}");
    assert!(
        error
            .lines()
            .any(|line| line
                == "reagent: profile none, provider codex-sub, credential login codex-sub"),
        "{error}"
    );
    // The login signed the list request in, and its token never shows.
    assert!(requests.recv().await.is_some());
    assert!(!error.contains("at-models-test"));
}

#[tokio::test]
async fn named_account_models_without_a_login_names_the_login_command() {
    let home = Home::with_config("[providers.codex-sub]\npreset = \"chatgpt\"\n").await;
    let state_home = home.dir.path().join("state");
    let output = home
        .run_env(
            &["models", "codex-sub"],
            &[("XDG_STATE_HOME", state_home.to_str().expect("utf-8 path"))],
        )
        .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("codex-sub needs a login; run reagent login chatgpt --as codex-sub"),
        "{stderr}"
    );
}
