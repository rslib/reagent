//! A fake MCP server and its message log, shared by the client tests.
#![allow(dead_code)]

pub mod http;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use reagent_mcp::StdioSpec;
use serde_json::{Value, json};
use tokio::time::sleep;

/// How long a test waits for the fake to log something.
const WAIT: Duration = Duration::from_secs(3);

/// How often a test looks again.
const POLL: Duration = Duration::from_millis(10);

/// One fake server process with the folder that holds its log.
pub struct Fake {
    /// The spec that starts the fake with its configuration.
    pub spec: StdioSpec,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fake {
    /// A fake configured by `config`, with a log path added to it.
    pub fn new(config: Value) -> Self {
        let dir = tempfile::tempdir().expect("a temp folder");
        let log = dir.path().join("log");
        let mut config = if config.is_null() { json!({}) } else { config };
        config["log"] = json!(log.display().to_string());
        let spec = StdioSpec {
            command: env!("CARGO_BIN_EXE_reagent-mcp-fake").to_owned(),
            args: Vec::new(),
            env: vec![("MCP_FAKE".to_owned(), config.to_string())],
            cwd: None,
        };
        Self {
            spec,
            log,
            _dir: dir,
        }
    }

    /// Every line the fake wrote, parsed.
    async fn lines(&self) -> Vec<Value> {
        let text = tokio::fs::read_to_string(&self.log)
            .await
            .unwrap_or_default();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    /// The messages the fake received, in order.
    pub async fn messages(&self) -> Vec<Value> {
        self.lines()
            .await
            .into_iter()
            .filter(|message| message.get("jsonrpc").is_some())
            .collect()
    }

    /// The `method` of each received message that has one, in order.
    pub async fn methods(&self) -> Vec<String> {
        self.messages()
            .await
            .iter()
            .filter_map(|message| message["method"].as_str().map(str::to_owned))
            .collect()
    }

    /// The first message with `method`.
    pub async fn method(&self, method: &str) -> Value {
        self.wait_for(|message| message["method"] == method)
            .await
            .unwrap_or_else(|| panic!("no {method} in the log"))
    }

    /// The first message matching `want`, waiting for it to arrive.
    pub async fn wait_for(&self, want: impl Fn(&Value) -> bool) -> Option<Value> {
        let start = Instant::now();
        loop {
            if let Some(found) = self.messages().await.into_iter().find(|line| want(line)) {
                return Some(found);
            }
            if start.elapsed() > WAIT {
                return None;
            }
            sleep(POLL).await;
        }
    }

    /// A pid the fake logged under `key`, such as `server_pid`.
    pub async fn pid(&self, key: &str) -> i32 {
        let start = Instant::now();
        loop {
            let found = self
                .lines()
                .await
                .into_iter()
                .find_map(|line| line.get(key).and_then(Value::as_i64));
            if let Some(pid) = found {
                return pid as i32;
            }
            assert!(start.elapsed() < WAIT, "no {key} in the log");
            sleep(POLL).await;
        }
    }
}

/// Whether the process `pid` exists.
pub fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Whether `pid` is gone within the shared wait.
pub async fn gone(pid: i32) -> bool {
    let start = Instant::now();
    while start.elapsed() < WAIT {
        if !alive(pid) {
            return true;
        }
        sleep(POLL).await;
    }
    false
}
