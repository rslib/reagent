//! A fake MCP server over stdio, driven by the `MCP_FAKE` environment variable.
//!
//! A test helper: pipes and threads only, so it needs no async runtime.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// The modern revision the fake reports.
const MODERN: &str = "2026-07-28";
/// The version a legacy fake reports when `version` is absent.
const LEGACY: &str = "2025-11-25";
/// The id of the `ping` request the fake sends before a tool list.
const PING_ID: u64 = 9001;
/// How many tools one page of a paged tool list holds.
const PAGE: usize = 2;
/// How long the fake says its tool list stays fresh.
const TTL_MS: u64 = 1_800_000;

/// The bytes of one MiB.
const MIB: usize = 1024 * 1024;

/// What the fake does, read from `MCP_FAKE`.
#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    era: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    pages: usize,
    #[serde(default)]
    tools: Vec<ToolConfig>,
    #[serde(default)]
    list_changed_after_call: bool,
    #[serde(default)]
    spawn_child: bool,
    #[serde(default)]
    ping_before_tools_list: bool,
    /// The versions a `-32022` answer to `server/discover` lists.
    #[serde(default)]
    discover_supported: Option<Vec<String>>,
    /// The ttl of a modern tool list, in milliseconds.
    #[serde(default)]
    ttl_ms: Option<u64>,
    /// How long the fake says nothing after it starts, in milliseconds.
    #[serde(default)]
    start_delay_ms: Option<u64>,
    /// How many MiB one line written at start holds, past the client's cap.
    #[serde(default)]
    huge_line_mib: Option<usize>,
    #[serde(default)]
    log: Option<String>,
}

/// One tool the fake offers, with the error a call to it returns.
#[derive(Clone, Debug, Deserialize)]
struct ToolConfig {
    name: String,
    #[serde(default)]
    error: Option<String>,
    /// The `annotations` of its list entry.
    #[serde(default)]
    annotations: Option<Value>,
    /// The `outputSchema` of its list entry.
    #[serde(default)]
    output_schema: Option<Value>,
}

/// The server state shared by the main loop and the sleep threads.
struct Fake {
    config: Config,
    out: Mutex<std::io::Stdout>,
    log: Option<Mutex<std::fs::File>>,
    changed: AtomicBool,
}

fn main() {
    let fake = Arc::new(Fake::start(read_config()));
    if let Some(ms) = fake.config.start_delay_ms {
        std::thread::sleep(Duration::from_millis(ms));
    }
    if fake.config.spawn_child {
        fake.spawn_child();
    }
    if let Some(mib) = fake.config.huge_line_mib {
        fake.huge_line(mib);
    }
    let mut waiting: Option<(Value, String)> = None;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        fake.log(&line);
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        match (message["method"].as_str(), id) {
            (Some(method), Some(id)) => {
                if method == "tools/list" && fake.config.ping_before_tools_list && waiting.is_none()
                {
                    let cursor = message["params"]["cursor"]
                        .as_str()
                        .unwrap_or("0")
                        .to_owned();
                    waiting = Some((id, cursor));
                    fake.send(json!({"jsonrpc": "2.0", "id": PING_ID, "method": "ping"}));
                } else {
                    fake.answer(method, &id, &message);
                }
            }
            (None, Some(id)) if id == json!(PING_ID) => {
                if let Some((pending, cursor)) = waiting.take() {
                    fake.list(&pending, &cursor);
                }
            }
            _ => {}
        }
    }
}

/// Reads the configuration, an empty one when the variable is absent or broken.
fn read_config() -> Config {
    std::env::var("MCP_FAKE")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

impl Fake {
    fn start(config: Config) -> Self {
        let log = config
            .log
            .as_deref()
            .and_then(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
            })
            .map(Mutex::new);
        let fake = Self {
            config,
            out: Mutex::new(std::io::stdout()),
            log,
            changed: AtomicBool::new(false),
        };
        fake.log_value(&json!({"server_pid": std::process::id()}));
        fake
    }

    /// Reports whether the fake speaks the modern era.
    fn modern(&self) -> bool {
        self.config.era.as_deref() != Some("legacy")
    }

    /// The version the fake reports in a handshake.
    fn version(&self) -> &str {
        self.config
            .version
            .as_deref()
            .unwrap_or(if self.modern() { MODERN } else { LEGACY })
    }

    /// Starts a child that a test can look for after the client stops.
    fn spawn_child(&self) {
        let spawned = std::process::Command::new("sleep")
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => self.log_value(&json!({"child_pid": child.id()})),
            Err(error) => self.log_value(&json!({"child_error": error.to_string()})),
        }
    }

    /// Appends one received line to the log.
    fn log(&self, line: &str) {
        let Some(log) = &self.log else { return };
        let mut log = log.lock().unwrap_or_else(|error| error.into_inner());
        let _ = writeln!(log, "{line}");
        let _ = log.flush();
    }

    /// Appends one line the fake wrote about itself to the log.
    fn log_value(&self, value: &Value) {
        self.log(&value.to_string());
    }

    /// Writes one message to stdout.
    fn send(&self, message: Value) {
        let mut out = self.out.lock().unwrap_or_else(|error| error.into_inner());
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    /// Sends a notification to the client.
    fn notify(&self, method: &str) {
        self.send(json!({"jsonrpc": "2.0", "method": method}));
    }

    /// Writes one line of `mib` MiB, which is past the client's read cap.
    fn huge_line(&self, mib: usize) {
        let mut out = self.out.lock().unwrap_or_else(|error| error.into_inner());
        let filler = vec![b'x'; mib * MIB];
        let _ = out.write_all(&filler);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }

    /// Sends the result of the request `id`.
    fn result(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    /// Sends a JSON-RPC error for the request `id`.
    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }));
    }

    /// Handles one request from the client.
    fn answer(self: &Arc<Self>, method: &str, id: &Value, message: &Value) {
        match method {
            "server/discover" => {
                if let Some(supported) = &self.config.discover_supported {
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32022,
                            "message": "unsupported protocol version",
                            "data": {"supported": supported},
                        },
                    }));
                } else if self.modern() {
                    self.result(
                        id,
                        json!({
                            "resultType": "complete",
                            "supportedVersions": [MODERN],
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fake", "version": "0"},
                        }),
                    );
                } else {
                    self.error(id, -32601, "method not found: server/discover");
                }
            }
            "initialize" => {
                if self.modern() {
                    self.error(id, -32601, "method not found: initialize");
                } else {
                    let version = self.version();
                    self.result(
                        id,
                        json!({
                            "protocolVersion": version,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fake", "version": "0"},
                        }),
                    );
                }
            }
            "tools/list" => {
                let cursor = message["params"]["cursor"].as_str().unwrap_or("0");
                self.list(id, cursor);
            }
            "tools/call" => self.call(id, message),
            other => self.error(id, -32601, &format!("method not found: {other}")),
        }
    }

    /// Sends one page of the tool list.
    fn list(&self, id: &Value, cursor: &str) {
        let tools = self.tools();
        let (start, end) = if self.config.pages > 0 {
            let page = cursor.parse::<usize>().unwrap_or(0);
            let start = (page * PAGE).min(tools.len());
            (start, (start + PAGE).min(tools.len()))
        } else {
            (0, tools.len())
        };
        let entries: Vec<Value> = tools[start..end]
            .iter()
            .map(|tool| {
                let mut entry = json!({
                    "name": tool.name,
                    "description": format!("the {} tool", tool.name),
                    "inputSchema": {"type": "object"},
                });
                if let Some(annotations) = &tool.annotations {
                    entry["annotations"] = annotations.clone();
                }
                if let Some(schema) = &tool.output_schema {
                    entry["outputSchema"] = schema.clone();
                }
                entry
            })
            .collect();
        let mut result = json!({"tools": entries});
        if self.modern() {
            result["resultType"] = json!("complete");
            result["ttlMs"] = json!(self.config.ttl_ms.unwrap_or(TTL_MS));
        }
        if end < tools.len() {
            result["nextCursor"] = json!((start / PAGE + 1).to_string());
        }
        self.result(id, result);
    }

    /// The tool list, as configured and after a list change.
    fn tools(&self) -> Vec<ToolConfig> {
        let mut tools = if self.config.pages > 0 {
            (1..=self.config.pages * PAGE)
                .map(|number| ToolConfig {
                    name: format!("tool-{number}"),
                    error: None,
                    annotations: None,
                    output_schema: None,
                })
                .collect()
        } else if self.config.tools.is_empty() {
            vec![ToolConfig {
                name: "echo".to_owned(),
                error: None,
                annotations: None,
                output_schema: None,
            }]
        } else {
            self.config.tools.clone()
        };
        if self.changed.load(Ordering::Relaxed) {
            tools.push(ToolConfig {
                name: "added".to_owned(),
                error: None,
                annotations: None,
                output_schema: None,
            });
        }
        tools
    }

    /// Calls one tool, waiting off the main loop for `sleep`.
    fn call(self: &Arc<Self>, id: &Value, message: &Value) {
        let name = message["params"]["name"].as_str().unwrap_or_default();
        let arguments = message["params"]["arguments"].clone();
        if name == "sleep" {
            let fake = Arc::clone(self);
            let id = id.clone();
            let ms = arguments["ms"].as_u64().unwrap_or(0);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                fake.result(&id, json!({"content": [{"type": "text", "text": "slept"}]}));
            });
            self.changed_after_call();
            return;
        }
        let error = self
            .config
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .and_then(|tool| tool.error.clone());
        let text = match (name, error) {
            ("echo", _) => arguments.to_string(),
            ("structured", _) => "ok".to_owned(),
            (_, Some(error)) => {
                self.changed_after_call();
                self.result(
                    id,
                    json!({
                        "content": [{"type": "text", "text": error}],
                        "isError": true,
                    }),
                );
                return;
            }
            (_, None) => format!("the {name} tool ran"),
        };
        let mut result = json!({"content": [{"type": "text", "text": text}]});
        if name == "structured" {
            result["structuredContent"] = json!({"b": 2});
        }
        self.result(id, result);
        self.changed_after_call();
    }

    /// Tells the client its tool list changed, once.
    fn changed_after_call(&self) {
        if self.config.list_changed_after_call && !self.changed.swap(true, Ordering::Relaxed) {
            self.notify("notifications/tools/list_changed");
        }
    }
}
