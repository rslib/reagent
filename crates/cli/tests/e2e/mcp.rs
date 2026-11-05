//! End-to-end tests for `reagent mcp add`, `remove`, `list` and `logout`.
//!
//! Each test runs the binary against a temp config and state folder, and the
//! list test answers over a fake HTTP server on a local port.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Output, Stdio};

use reagent_config::McpTransport;
use reagent_config::Secret;
use reagent_config::mcp::McpDirect;
use reagent_mcp::auth::{self, Store, Token};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::oneshot;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// The modern revision the fake server reports.
const MODERN: &str = "2026-07-28";

/// A temp home the command writes its config and state in.
struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            root: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// The global config folder the command reads and writes.
    fn config(&self) -> PathBuf {
        self.root.path().join("config/reagent")
    }

    /// The state folder the logins are saved in.
    fn state(&self) -> PathBuf {
        self.root.path().join("state/reagent")
    }

    /// The global `mcp.toml`.
    fn file(&self) -> PathBuf {
        self.config().join("mcp.toml")
    }

    /// The `mcp.toml` of `profile`.
    fn profile(&self, profile: &str) -> PathBuf {
        self.config()
            .join("profiles")
            .join(format!("{profile}.mcp.toml"))
    }

    async fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("ANTHROPIC_API_KEY", "test-key")
            .env_remove("REAGENT_PROFILE")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .expect("runs reagent")
    }
}

/// The text of an output stream.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The reply the fake provider sends: one text delta and the end of the turn.
const REPLY: &str = concat!(
    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);

/// Serves one Anthropic request with `REPLY` and records its body.
async fn provider() -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("the bound address");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let Some(body) = read_body(&mut socket).await else {
            return;
        };
        let _ = sender.send(body);
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            REPLY.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(REPLY.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{address}"), receiver)
}

/// The names of the tools the recorded request offers.
async fn request_tools(recorded: oneshot::Receiver<String>) -> Vec<String> {
    let body = recorded.await.expect("the request");
    let body: Value = serde_json::from_str(&body).expect("a JSON body");
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Writes `text` as the global `mcp.toml`.
async fn write_config(home: &Home, text: &str) {
    tokio::fs::create_dir_all(home.config())
        .await
        .expect("makes the config folder");
    tokio::fs::write(home.file(), text)
        .await
        .expect("writes mcp.toml");
    tokio::fs::set_permissions(home.file(), std::fs::Permissions::from_mode(0o600))
        .await
        .expect("makes it private");
}

/// The text `path` holds.
async fn read(path: &std::path::Path) -> String {
    tokio::fs::read_to_string(path)
        .await
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[tokio::test]
async fn mcp_cli_add_stdio() {
    let home = Home::new();
    let out = home
        .run(&[
            "mcp",
            "add",
            "fs",
            "--env",
            "OTHER=plain",
            "--env",
            "KEY=${K}",
            "--",
            "npx",
            "-y",
            "server-fs",
            ".",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout).trim(),
        format!("added fs to {}", home.file().display())
    );

    let written = read(&home.file()).await;
    assert!(written.contains("npx"), "{written}");
    assert!(written.contains("${K}"), "{written}");
    let mode = tokio::fs::metadata(&home.file())
        .await
        .expect("the file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "{written}");

    let servers = reagent_config::load_mcp(&home.config(), None).expect("loads the file");
    let fs = &servers.servers["fs"];
    let McpTransport::Stdio {
        command,
        args,
        env,
        cwd,
    } = &fs.transport
    else {
        panic!("not stdio: {:?}", fs.transport);
    };
    assert_eq!(command, "npx");
    assert_eq!(args, &["-y", "server-fs", "."]);
    assert_eq!(
        env,
        &[
            ("KEY".to_string(), "${K}".to_string()),
            ("OTHER".to_string(), "plain".to_string()),
        ]
    );
    assert_eq!(cwd, &None);
    assert!(fs.enabled);
}

#[tokio::test]
async fn mcp_cli_add_client_secret_env_keeps_the_secret_out_of_argv() {
    let home = Home::new();
    let out = home
        .run(&[
            "mcp",
            "add",
            "docs",
            "--url",
            "https://x/mcp",
            "--client-id",
            "client-1",
            "--client-secret-env",
            "DOCS_SECRET",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stderr), "");
    let written = read(&home.file()).await;
    assert!(written.contains("${DOCS_SECRET}"), "{written}");
    let servers = reagent_config::load_mcp(&home.config(), None).expect("loads the file");
    let oauth = servers.servers["docs"]
        .oauth
        .clone()
        .expect("an oauth table");
    let secret = oauth.client_secret.expect("a client secret");
    assert_eq!(secret.expose(), "${DOCS_SECRET}");
}

#[tokio::test]
async fn mcp_cli_add_client_secret_env_refuses_a_bad_name() {
    let home = Home::new();
    let out = home
        .run(&[
            "mcp",
            "add",
            "docs",
            "--url",
            "https://x/mcp",
            "--client-secret-env",
            "1bad-name",
        ])
        .await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("invalid variable name 1bad-name"),
        "{}",
        text(&out.stderr)
    );
    assert!(!home.file().exists(), "a bad name wrote a file");
}

#[tokio::test]
async fn mcp_cli_add_client_secret_names_the_shell_history() {
    let home = Home::new();
    let out = home
        .run(&[
            "mcp",
            "add",
            "docs",
            "--url",
            "https://x/mcp",
            "--client-secret",
            "hunter2",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let note = text(&out.stderr);
    assert!(note.contains("shell history"), "{note}");
    assert!(note.contains("--client-secret-env"), "{note}");
    // The value is written, and never printed.
    assert!(!note.contains("hunter2"), "{note}");
    assert!(
        !text(&out.stdout).contains("hunter2"),
        "{}",
        text(&out.stdout)
    );
    assert!(read(&home.file()).await.contains("hunter2"));
}

#[tokio::test]
async fn mcp_cli_add_http_to_profile() {
    let home = Home::new();
    let out = home
        .run(&[
            "mcp",
            "add",
            "docs",
            "--profile",
            "work",
            "--url",
            "https://x/mcp",
            "--header",
            "X-Zed: 1",
            "--bearer-token-env-var",
            "DOCS_TOKEN",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout).trim(),
        format!("added docs to {}", home.profile("work").display())
    );
    let written = read(&home.profile("work")).await;
    assert!(written.contains("${DOCS_TOKEN}"), "{written}");
    assert!(!home.file().exists(), "the global file was touched");

    let servers = reagent_config::load_mcp(&home.config(), Some("work")).expect("loads the file");
    let McpTransport::Http { url, headers } = &servers.servers["docs"].transport else {
        panic!("not http");
    };
    assert_eq!(url, "https://x/mcp");
    assert_eq!(
        headers,
        &[
            (
                "Authorization".to_string(),
                "Bearer ${DOCS_TOKEN}".to_string()
            ),
            ("X-Zed".to_string(), "1".to_string()),
        ]
    );
}

#[tokio::test]
async fn mcp_cli_add_local_is_refused() {
    let home = Home::new();
    for flag in ["-l", "--local"] {
        let out = home
            .run(&["mcp", "add", "fs", flag, "--", "npx", "-y", "server-fs"])
            .await;
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
        assert!(
            text(&out.stderr).contains(
                "reagent keeps MCP servers out of repo files; add it to mcp.toml or a profile"
            ),
            "{}",
            text(&out.stderr)
        );
        assert!(!home.file().exists(), "{flag} wrote a file");
    }
}

#[tokio::test]
async fn mcp_cli_remove() {
    let home = Home::new();
    let out = home
        .run(&["mcp", "add", "fs", "--", "npx", "-y", "server-fs"])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));

    let out = home.run(&["mcp", "remove", "fs"]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout).trim(),
        format!("removed fs from {}", home.file().display())
    );
    let servers = reagent_config::load_mcp(&home.config(), None).expect("loads the file");
    assert!(servers.servers.is_empty(), "{servers:?}");

    let again = home.run(&["mcp", "remove", "fs"]).await;
    assert_eq!(again.status.code(), Some(1), "{}", text(&again.stdout));
    let message = text(&again.stderr);
    assert!(message.contains("fs"), "{message}");
    assert!(message.contains("mcp.toml"), "{message}");
}

#[tokio::test]
async fn mcp_cli_list_reports_states() {
    let home = Home::new();
    let fake = Fake::start(&["alpha", "beta"]).await;
    let out = home.run(&["mcp", "add", "ok", "--url", &fake.url]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = home
        .run(&[
            "mcp",
            "add",
            "bad",
            "--env",
            "SECRET=hunter2",
            "--",
            "no-such-command-reagent-test",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));

    let out = home.run(&["mcp", "list"]).await;
    let listing = text(&out.stdout);
    assert!(listing.contains("ok  ready"), "{listing}");
    assert!(listing.contains("2 tools: alpha, beta"), "{listing}");
    assert!(listing.contains("bad  failed"), "{listing}");
    assert!(listing.contains("error:"), "{listing}");
    assert!(!listing.contains("hunter2"), "{listing}");
    assert!(
        !text(&out.stderr).contains("hunter2"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1), "{listing}");
}

#[tokio::test]
async fn mcp_direct_cli_add() {
    let home = Home::new();
    let out = home
        .run(&["mcp", "add", "demo", "--direct-tool", "echo", "--", "cmd"])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let written = read(&home.file()).await;
    assert!(written.contains("direct = [\"echo\"]"), "{written}");
    let servers = reagent_config::load_mcp(&home.config(), None).expect("loads the file");
    assert_eq!(
        servers.servers["demo"].direct,
        McpDirect::Only(vec!["echo".to_string()])
    );

    // Both flags at once is an error, and writes nothing.
    let out = home
        .run(&[
            "mcp",
            "add",
            "both",
            "--direct",
            "--direct-tool",
            "echo",
            "--",
            "cmd",
        ])
        .await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("not both"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        !read(&home.file()).await.contains("both"),
        "an entry written"
    );

    // `--direct` writes `direct = true`.
    let out = home
        .run(&["mcp", "add", "plain", "--direct", "--", "cmd"])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let servers = reagent_config::load_mcp(&home.config(), None).expect("loads the file");
    assert_eq!(servers.servers["plain"].direct, McpDirect::All);
    assert!(read(&home.file()).await.contains("direct = true"));
}

#[tokio::test]
async fn mcp_direct_in_request() {
    let home = Home::new();
    let fake = Fake::start(&["echo", "sleep"]).await;
    write_config(
        &home,
        &format!(
            "[servers.demo]\nurl = \"{}\"\ndirect = [\"echo\"]\n",
            fake.url
        ),
    )
    .await;

    let (base, recorded) = provider().await;
    let out = home
        .run(&["--provider", "anthropic", "--base-url", &base, "-p", "hi"])
        .await;
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let tools = request_tools(recorded).await;
    assert!(tools.contains(&"mcp__demo__echo".to_string()), "{tools:?}");
    assert!(
        !tools.contains(&"mcp__demo__sleep".to_string()),
        "{tools:?}"
    );
}

#[tokio::test]
async fn mcp_direct_without_codemode() {
    let home = Home::new();
    let fake = Fake::start(&["echo", "sleep"]).await;
    write_config(
        &home,
        &format!("[servers.demo]\nurl = \"{}\"\ndirect = true\n", fake.url),
    )
    .await;

    let (base, recorded) = provider().await;
    let out = home
        .run(&[
            "--provider",
            "anthropic",
            "--base-url",
            &base,
            "-p",
            "hi",
            "--tools",
            "-codemode",
        ])
        .await;
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("mcp demo ready (2 tools)"), "{stderr}");
    assert!(!stderr.contains("need codemode"), "{stderr}");
    let tools = request_tools(recorded).await;
    assert!(tools.contains(&"mcp__demo__echo".to_string()), "{tools:?}");
    assert!(tools.contains(&"mcp__demo__sleep".to_string()), "{tools:?}");
    assert!(!tools.contains(&"codemode".to_string()), "{tools:?}");
}

#[tokio::test]
async fn mcp_cli_logout() {
    let home = Home::new();
    let mut store = Store::default();
    store.put(
        "docs",
        "https://issuer.example",
        Token {
            client_id: "client-1".to_string(),
            client_secret: None,
            access: Secret::from("access-secret"),
            refresh: None,
            expires_at: u64::MAX,
            scopes: Vec::new(),
            needed: Vec::new(),
            resource: "https://x/mcp".to_string(),
            redirect_uri: String::new(),
        },
    );
    auth::write(&home.state(), &store).expect("writes the login");

    let out = home.run(&["mcp", "logout", "docs"]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim(), "logged out of docs");
    assert!(auth::read(&home.state()).expect("reads").is_empty());

    let again = home.run(&["mcp", "logout", "docs"]).await;
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert_eq!(text(&again.stdout).trim(), "no login for docs");
}

/// A fake MCP server over streamable HTTP: the modern probe and the tool list.
struct Fake {
    /// The endpoint the command posts its messages to.
    url: String,
}

impl Fake {
    /// Starts a server that lists `tools` and answers nothing else.
    async fn start(tools: &[&str]) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let address = listener.local_addr().expect("the bound address");
        let listed: Vec<Value> = tools
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "description": format!("the {name} tool"),
                    "inputSchema": {"type": "object"},
                })
            })
            .collect();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let listed = listed.clone();
                tokio::spawn(async move { serve(socket, &listed).await });
            }
        });
        Fake {
            url: format!("http://{address}/mcp"),
        }
    }
}

/// Reads one message and answers it.
async fn serve(mut socket: TcpStream, tools: &[Value]) {
    let Some(body) = read_body(&mut socket).await else {
        return;
    };
    let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let id = message.get("id").cloned().filter(|id| !id.is_null());
    let Some(id) = id else {
        reply(&mut socket, 202, "").await;
        return;
    };
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "server/discover" => json!({
            "resultType": "complete",
            "supportedVersions": [MODERN],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "0"},
        }),
        "tools/list" => json!({"tools": tools, "resultType": "complete", "ttlMs": 600_000}),
        other => {
            let error = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {other}")},
            });
            reply(&mut socket, 200, &error.to_string()).await;
            return;
        }
    };
    let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
    reply(&mut socket, 200, &answer.to_string()).await;
}

/// The body of one request, once its head and body have arrived.
async fn read_body(socket: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim() == "content-length")
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= end + 4 + length {
                let body = buffer[end + 4..end + 4 + length].to_vec();
                return Some(String::from_utf8_lossy(&body).into_owned());
            }
        }
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Writes one response and closes the connection.
async fn reply(socket: &mut TcpStream, status: u16, body: &str) {
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.shutdown().await;
}
