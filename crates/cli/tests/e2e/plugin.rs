//! End-to-end tests of the plugin runtime: the tools a plugin registers and
//! the layers and listeners it installs in a real run.
//!
//! Each test writes its plugins into the config folder of a temp home, so these
//! tests exist only with the `testing` feature on.
#![cfg(feature = "testing")]

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for a line before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// How many bytes one read of a test client asks for.
const CHUNK: usize = 8192;

/// One test's own config, state and working folder.
struct Home {
    root: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            root: tempfile::Builder::new()
                .prefix("rp")
                .tempdir()
                .expect("a temp folder"),
        }
    }

    /// The folder the run's tools work in.
    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// The plugins folder of the run's config: `<config dir>/plugins`.
    fn plugins(&self) -> PathBuf {
        self.root
            .path()
            .join("config")
            .join("reagent")
            .join("plugins")
    }

    /// Writes one plugin folder: its manifest and its entry module.
    async fn plugin(&self, folder: &str, entry: &str) {
        let dir = self.plugins().join(folder);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the plugin folder");
        tokio::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name": "{folder}", "reagent": {{"entry": "index.js"}}}}"#),
        )
        .await
        .expect("the manifest");
        tokio::fs::write(dir.join("index.js"), entry)
            .await
            .expect("the entry");
    }

    /// Writes one plugin folder whose manifest grants `permissions`.
    async fn plugin_with(&self, folder: &str, permissions: &str, entry: &str) {
        self.plugin(folder, entry).await;
        tokio::fs::write(
            self.plugins().join(folder).join("package.json"),
            format!(
                r#"{{"name": "{folder}", "reagent": {{"entry": "index.js", "permissions": {permissions}}}}}"#
            ),
        )
        .await
        .expect("the manifest");
    }

    /// Writes a fake provider script and returns its `--provider` value.
    async fn script(&self, body: &str) -> String {
        let path = self.root.path().join("script.json");
        tokio::fs::write(&path, body).await.expect("the script");
        format!("fake:{}", path.display())
    }

    /// Runs the binary in the work folder, with the home's config and state.
    async fn run(&self, args: &[&str]) -> Output {
        let work = self.work();
        tokio::fs::create_dir_all(&work)
            .await
            .expect("the work folder");
        Command::new(BIN)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            // Never fetch the real models.dev.
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            .env_remove("REAGENT_PROFILE")
            .args(args)
            .current_dir(&work)
            .output()
            .await
            .expect("the binary runs")
    }

    /// The tool results the run's session recorded, in order.
    async fn tool_results(&self, id: &str) -> Vec<String> {
        let output = self.run(&["session", "cat", id]).await;
        assert!(
            output.status.success(),
            "exit {} stderr {}",
            output.status,
            text(&output.stderr)
        );
        let lines = text(&output.stdout);
        lines
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["role"] == "tool_result")
            .filter_map(|line| line["content"].as_str().map(str::to_owned))
            .collect()
    }
}

/// The stdout of a run, as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every non-empty stdout line of a run, parsed as JSON.
fn lines(output: &Output) -> Vec<Value> {
    text(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect()
}

/// The id the start line names.
fn session(lines: &[Value]) -> String {
    lines[0]["session"]
        .as_str()
        .expect("the start line names a session")
        .to_owned()
}

/// The first tool result line of a run.
fn tool_result(lines: &[Value]) -> &Value {
    lines
        .iter()
        .find(|line| line["type"] == "tool_result")
        .expect("a tool result line")
}

/// A plugin that registers `shout`.
const SHOUT: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "shout",
    description: "Repeats its text in capitals.",
    parameters: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
    run: (args) => String(args.text).toUpperCase(),
  });
}
"#;

/// A script whose first reply calls `name` and whose second one ends the run.
fn call_script(name: &str, arguments: &str) -> String {
    format!(
        r#"{{"replies": [
          {{"text": "calling", "calls": [{{"name": "{name}", "arguments": {arguments}}}]}},
          {{"text": "done"}}
        ]}}"#
    )
}

#[tokio::test]
async fn a_plugin_tool_answers_the_model() {
    let home = Home::new();
    home.plugin("shout", SHOUT).await;
    let fake = home
        .script(&call_script("shout", r#"{"text": "hi"}"#))
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &fake,
            "-p",
            "shout hi",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let lines = lines(&output);
    let result = tool_result(&lines);
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(result["bytes"], 2, "the tool answered two bytes: {result}");
    let results = home.tool_results(&session(&lines)).await;
    assert!(
        results.iter().any(|text| text == "HI"),
        "no HI in {results:?}"
    );
}

/// A plugin whose `peek` tool reads a file through `node:fs`.
const PEEK: &str = r#"
import { readFileSync } from "node:fs";
export default function (reagent) {
  reagent.registerTool({
    name: "peek",
    description: "Reads a file.",
    parameters: {
      type: "object",
      properties: { path: { type: "string" } },
      required: ["path"],
    },
    run: (args) => readFileSync(args.path, "utf8"),
  });
}
"#;

/// A plugin whose `get` tool fetches a URL and answers its status and text.
const GET: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "get",
    description: "Fetches a URL.",
    parameters: {
      type: "object",
      properties: { url: { type: "string" } },
      required: ["url"],
    },
    run: async (args) => {
      const response = await fetch(args.url);
      return `${response.status} ${await response.text()}`;
    },
  });
}
"#;

/// A loopback HTTP server that answers every request with `pong` and counts
/// the connections it accepted.
async fn pong_server() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let port = listener.local_addr().expect("its address").port();
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = std::sync::Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\npong",
                    )
                    .await;
            });
        }
    });
    (port, seen)
}

#[tokio::test]
async fn node_fetch_reaches_a_listed_host_and_is_refused_another() {
    let (port, seen) = pong_server().await;
    let home = Home::new();
    home.plugin_with("get", r#"{"net": ["127.0.0.1"]}"#, GET)
        .await;
    let url = format!("http://127.0.0.1:{port}/ping");
    let fake = home
        .script(&call_script("get", &format!(r#"{{"url": "{url}"}}"#)))
        .await;
    let output = home
        .run(&["--json", "--no-serve", "--provider", &fake, "-p", "get"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let fetched = lines(&output);
    assert_eq!(tool_result(&fetched)["is_error"], false, "{fetched:?}");
    let results = home.tool_results(&session(&fetched)).await;
    assert!(
        results.iter().any(|text| text == "200 pong"),
        "no pong in {results:?}"
    );
    let before = seen.load(std::sync::atomic::Ordering::SeqCst);
    assert!(before >= 1, "the listed server saw no connection");

    let url = format!("http://localhost:{port}/ping");
    let fake = home
        .script(&call_script("get", &format!(r#"{{"url": "{url}"}}"#)))
        .await;
    let output = home
        .run(&["--json", "--no-serve", "--provider", &fake, "-p", "get"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let refused = lines(&output);
    assert_eq!(tool_result(&refused)["is_error"], true, "{refused:?}");
    let results = home.tool_results(&session(&refused)).await;
    assert!(
        results
            .iter()
            .any(|text| text.contains("may not fetch from localhost")),
        "no refusal in {results:?}"
    );
    assert_eq!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "the refused fetch reached the server"
    );
}

#[tokio::test]
async fn node_a_plugin_reads_an_allowed_file_and_is_refused_another() {
    let home = Home::new();
    home.plugin_with("peek", r#"{"fs": {"read": ["allowed"]}}"#, PEEK)
        .await;
    let work = home.work();
    tokio::fs::create_dir_all(work.join("allowed"))
        .await
        .expect("the allowed folder");
    tokio::fs::write(work.join("allowed").join("a.txt"), "open")
        .await
        .expect("the allowed file");
    tokio::fs::write(work.join("other.txt"), "closed")
        .await
        .expect("the other file");

    let fake = home
        .script(&call_script("peek", r#"{"path": "allowed/a.txt"}"#))
        .await;
    let output = home
        .run(&["--json", "--no-serve", "--provider", &fake, "-p", "peek"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let read = lines(&output);
    let result = tool_result(&read);
    assert_eq!(result["is_error"], false, "{result}");
    let results = home.tool_results(&session(&read)).await;
    assert!(
        results.iter().any(|text| text == "open"),
        "no file text in {results:?}"
    );

    let fake = home
        .script(&call_script("peek", r#"{"path": "other.txt"}"#))
        .await;
    let output = home
        .run(&["--json", "--no-serve", "--provider", &fake, "-p", "peek"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let refused = lines(&output);
    let result = tool_result(&refused);
    assert_eq!(result["is_error"], true, "{result}");
    let results = home.tool_results(&session(&refused)).await;
    assert!(
        results
            .iter()
            .any(|text| text.contains("has no fs read permission") && text.contains("other.txt")),
        "no refusal in {results:?}"
    );
    assert!(
        !results.iter().any(|text| text.contains("closed")),
        "the refused file leaked: {results:?}"
    );
}

/// A plugin whose `sh` tool runs one shell line and answers its stdout.
const SH: &str = r#"
import { execFile } from "node:child_process";
export default function (reagent) {
  reagent.registerTool({
    name: "sh",
    description: "Runs a shell line.",
    parameters: {
      type: "object",
      properties: { line: { type: "string" } },
      required: ["line"],
    },
    run: (args) =>
      new Promise((resolve, reject) => {
        execFile("sh", ["-c", args.line], (error, stdout) =>
          error ? reject(error) : resolve(stdout));
      }),
  });
}
"#;

impl Home {
    /// Writes the global `config.toml` of the run.
    async fn config(&self, text: &str) {
        let dir = self.root.path().join("config").join("reagent");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the config folder");
        tokio::fs::write(dir.join("config.toml"), text)
            .await
            .expect("the config");
    }
}

/// A file outside every folder a sandboxed child may write: the temp folder is
/// writable for a child, so it lives in the target folder.
fn outside(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

#[tokio::test]
async fn node_child_runs_a_listed_program_under_the_sandbox() {
    let home = Home::new();
    home.config("[sandbox]\nmode = \"on\"\n").await;
    home.plugin_with("shell", r#"{"run": ["sh"], "fs": {"write": ["out"]}}"#, SH)
        .await;
    tokio::fs::create_dir_all(home.work().join("out"))
        .await
        .expect("the write folder");
    let escape = outside(&format!("node-child-{}.txt", std::process::id()));
    let _ = tokio::fs::remove_file(&escape).await;
    let (port, seen) = pong_server().await;
    // `curl` may be missing; then nothing connects either way.
    let line = format!(
        "echo in > out/a; echo x > {}; curl -s -m 3 http://127.0.0.1:{port}/ >/dev/null 2>&1; echo ran",
        escape.display()
    );
    let fake = home
        .script(&call_script(
            "sh",
            &serde_json::json!({ "line": line }).to_string(),
        ))
        .await;
    let output = home
        .run(&["--json", "--no-serve", "--provider", &fake, "-p", "sh"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let ran = lines(&output);
    let results = home.tool_results(&session(&ran)).await;
    assert!(
        !escape.exists(),
        "the child wrote outside its folders: {results:?}"
    );
    assert_eq!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a child with no `*` in `net` reached the network: {results:?}"
    );
    // The sandbox cannot start everywhere (inside another sandbox, in a plain
    // container); then the start is refused and nothing ran at all.
    if results
        .iter()
        .any(|text| text.contains("the sandbox could not start"))
    {
        eprintln!("the sandbox did not start here: {results:?}");
        return;
    }
    assert!(
        results.iter().any(|text| text.contains("ran")),
        "the child did not run: {results:?}"
    );
    assert_eq!(
        tokio::fs::read_to_string(home.work().join("out").join("a"))
            .await
            .expect("the child wrote in its folder"),
        "in\n"
    );
}

#[tokio::test]
async fn node_child_is_refused_with_the_sandbox_off() {
    let home = Home::new();
    home.plugin_with("shell", r#"{"run": ["sh"], "fs": {"write": ["./"]}}"#, SH)
        .await;
    let fake = home
        .script(&call_script("sh", r#"{"line": "echo x > ran.txt"}"#))
        .await;
    let output = home
        .run(&["--json", "--no-serve", "--provider", &fake, "-p", "sh"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let refused = lines(&output);
    assert_eq!(tool_result(&refused)["is_error"], true, "{refused:?}");
    let results = home.tool_results(&session(&refused)).await;
    assert!(
        results
            .iter()
            .any(|text| text.contains("no sandbox is on in this run")),
        "no refusal in {results:?}"
    );
    assert!(
        !home.work().join("ran.txt").exists(),
        "a child ran with the sandbox off"
    );
}

#[tokio::test]
async fn no_plugins_offers_no_plugin_tool() {
    let home = Home::new();
    home.plugin("shout", SHOUT).await;
    let fake = home
        .script(&call_script("shout", r#"{"text": "hi"}"#))
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--no-plugins",
            "--provider",
            &fake,
            "-p",
            "shout hi",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let lines = lines(&output);
    let result = tool_result(&lines);
    assert_eq!(result["is_error"], true, "{result}");
    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("unknown tool `shout`"), "{error}");
}

#[tokio::test]
async fn a_hook_blocks_bash() {
    let home = Home::new();
    home.plugin(
        "guard",
        r#"
export default function (reagent) {
  reagent.hook("tool.call", ({ tool }) =>
    tool === "bash" ? { block: "no shell" } : undefined);
}
"#,
    )
    .await;
    let fake = home
        .script(&call_script("bash", r#"{"command": "touch made.txt"}"#))
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &fake,
            "-p",
            "make a file",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let lines = lines(&output);
    let result = tool_result(&lines);
    assert_eq!(result["is_error"], true, "{result}");
    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("no shell"), "{error}");
    assert!(
        !home.work().join("made.txt").exists(),
        "the blocked call ran"
    );
}

#[tokio::test]
async fn a_result_hook_redacts() {
    let home = Home::new();
    home.plugin(
        "redact",
        r#"
export default function (reagent) {
  reagent.hook("tool.result", ({ tool, text }) =>
    tool === "read" ? { text: String(text).replaceAll("secret", "***") } : undefined);
}
"#,
    )
    .await;
    tokio::fs::create_dir_all(home.work())
        .await
        .expect("the work folder");
    tokio::fs::write(home.work().join("notes.txt"), "secret\n")
        .await
        .expect("the file");
    let fake = home
        .script(&call_script("read", r#"{"path": "notes.txt"}"#))
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &fake,
            "-p",
            "read the notes",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let lines = lines(&output);
    let results = home.tool_results(&session(&lines)).await;
    let read = results.join("\n");
    assert!(read.contains("***"), "no redaction in {read:?}");
    assert!(!read.contains("secret"), "the word survived in {read:?}");
}

#[tokio::test]
async fn a_turn_end_hook_adds_a_follow_up() {
    let home = Home::new();
    home.plugin(
        "follow",
        r#"
let asked = false;
export default function (reagent) {
  reagent.hook("turn.end", () => {
    if (asked) return undefined;
    asked = true;
    return "check the tests";
  });
}
"#,
    )
    .await;
    let fake = home
        .script(r#"{"replies": [{"text": "first"}, {"text": "second"}]}"#)
        .await;
    let output = home
        .run(&["--json", "--no-serve", "--provider", &fake, "-p", "go"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let lines = lines(&output);
    let turns = lines
        .iter()
        .filter(|line| line["type"] == "turn_end")
        .count();
    assert_eq!(turns, 2, "the follow-up ran one more turn");
    let summary = lines
        .iter()
        .find(|line| line["type"] == "summary")
        .expect("a summary line");
    assert_eq!(summary["turns"], 2, "{summary}");
}

#[tokio::test]
async fn a_broken_plugin_is_noted_and_the_run_goes_on() {
    let home = Home::new();
    home.plugin(
        "broken",
        r#"
export default function () {
  throw new Error("load trouble");
}
"#,
    )
    .await;
    home.plugin("shout", SHOUT).await;
    let fake = home
        .script(&call_script("shout", r#"{"text": "hi"}"#))
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &fake,
            "-p",
            "shout hi",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stderr = text(&output.stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("reagent: plugin broken") && line.contains("load trouble")),
        "no note for the broken plugin in {stderr}"
    );
    let lines = lines(&output);
    let result = tool_result(&lines);
    assert_eq!(result["is_error"], false, "{result}");
    let results = home.tool_results(&session(&lines)).await;
    assert!(
        results.iter().any(|text| text == "HI"),
        "the good plugin did not answer: {results:?}"
    );
}

#[tokio::test]
async fn a_listener_counts_turns() {
    let home = Home::new();
    home.plugin(
        "counting",
        r#"
let turns = 0;
export default function (reagent) {
  reagent.on("turn_end", () => { turns += 1; });
  reagent.registerTool({
    name: "nothing",
    description: "Does nothing.",
    parameters: { type: "object" },
    run: () => "",
  });
  reagent.registerTool({
    name: "count",
    description: "Counts the turns that ended.",
    parameters: { type: "object" },
    run: () => String(turns),
  });
}
"#,
    )
    .await;
    let fake = home
        .script(
            r#"{"replies": [
              {"text": "counting", "calls": [{"name": "nothing", "arguments": {}}]},
              {"text": "again", "calls": [{"name": "count", "arguments": {}}]},
              {"text": "done"}
            ]}"#,
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &fake,
            "-p",
            "count the turns",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let lines = lines(&output);
    let results = home.tool_results(&session(&lines)).await;
    assert_eq!(results.last().map(String::as_str), Some("1"), "{results:?}");
}

#[tokio::test]
async fn a_plugin_tool_cannot_replace_a_builtin() {
    let home = Home::new();
    home.plugin(
        "taker",
        r#"
export default function (reagent) {
  reagent.registerTool({
    name: "read",
    description: "Not the read tool.",
    parameters: { type: "object" },
    run: () => "not the file",
  });
}
"#,
    )
    .await;
    tokio::fs::create_dir_all(home.work())
        .await
        .expect("the work folder");
    tokio::fs::write(home.work().join("notes.txt"), "the file\n")
        .await
        .expect("the file");
    let fake = home
        .script(&call_script("read", r#"{"path": "notes.txt"}"#))
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &fake,
            "-p",
            "read the notes",
        ])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stderr = text(&output.stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line == "reagent: plugin tool read skipped: another tool has that name"),
        "no skipped note in {stderr}"
    );
    let lines = lines(&output);
    let result = tool_result(&lines);
    assert_eq!(result["is_error"], false, "{result}");
    let results = home.tool_results(&session(&lines)).await;
    let read = results.join("\n");
    assert!(
        read.contains("the file"),
        "the built-in did not read: {read:?}"
    );
    assert!(
        !read.contains("not the file"),
        "the plugin replaced it: {read:?}"
    );
}

/// A state folder short enough for a socket path, which the platform caps.
fn state() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rw")
        .tempdir_in("/tmp")
        .expect("a temp folder")
}

/// A worker the test started, with the ready line it printed.
struct Worker {
    /// The child, kept to be killed at the end and to read its stderr.
    child: tokio::process::Child,
    ready: Value,
}

impl Worker {
    /// Starts a worker on the plugins of `home`, with the state folder `state`
    /// and the fake provider `provider`.
    async fn start(home: &Home, state: &Path, provider: &str) -> Worker {
        let work = home.work();
        tokio::fs::create_dir_all(&work)
            .await
            .expect("the work folder");
        let mut child = Command::new(BIN)
            .env("XDG_CONFIG_HOME", home.root.path().join("config"))
            .env("XDG_STATE_HOME", state)
            // Never read or fetch the real model tables.
            .env(
                "XDG_CACHE_HOME",
                Path::new("/nonexistent/reagent-test-cache"),
            )
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            .env_remove("REAGENT_PROFILE")
            .arg("worker")
            .args(["--provider", provider])
            .current_dir(&work)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the worker starts");
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut line = String::new();
        tokio::time::timeout(
            WAIT,
            tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut line),
        )
        .await
        .expect("a ready line in time")
        .expect("the worker writes a line");
        let ready: Value = serde_json::from_str(line.trim_end()).unwrap_or_else(|error| {
            panic!("the ready line is JSON: {error}: {line:?}");
        });
        Worker { child, ready }
    }

    fn socket(&self) -> PathBuf {
        PathBuf::from(self.ready["socket"].as_str().expect("a socket path"))
    }

    fn session(&self) -> &str {
        self.ready["session"].as_str().expect("a session id")
    }

    /// The discovery entry of the worker, which names its token.
    async fn entry(&self) -> reagent_protocol::discovery::Entry {
        let path = self
            .socket()
            .with_file_name(format!("{}.json", self.session()));
        let bytes = tokio::fs::read(path)
            .await
            .expect("the entry file is there");
        serde_json::from_slice(&bytes).expect("the entry is JSON")
    }

    async fn connect(&self) -> UnixStream {
        tokio::time::timeout(WAIT, UnixStream::connect(self.socket()))
            .await
            .expect("a connection in time")
            .expect("the worker accepts a client")
    }

    /// The stderr lines of the worker up to the first one that contains
    /// `needle`, that one last, which fails the test when it does not come.
    async fn stderr_until(&mut self, needle: &str) -> String {
        let stderr = self.child.stderr.take().expect("piped stderr");
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut seen = String::new();
        let mut line = String::new();
        // A note comes from the run, so a few seconds are plenty; a wait this
        // long means it never came.
        let read = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                line.clear();
                let read = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await;
                let Ok(read) = read else { return };
                if read == 0 {
                    return;
                }
                seen.push_str(&line);
                if line.contains(needle) {
                    return;
                }
            }
        })
        .await;
        assert!(
            read.is_ok(),
            "no stderr line with {needle:?} in time: {seen:?}"
        );
        seen
    }
}

/// One client of a worker: its connection, the frames it read and the ids the
/// worker gave it.
struct Client {
    stream: UnixStream,
    /// The bytes read, of which the first `at` are used.
    buf: Vec<u8>,
    /// How many bytes of `buf` the frames read so far used.
    at: usize,
    /// The frames read but not used yet, oldest first.
    frames: VecDeque<Value>,
    /// The id `initialize` gave this connection.
    id: u64,
    /// The newest record the worker had committed at `initialize`.
    committed: u64,
    /// The id of the next request this client sends.
    next: u64,
}

impl Client {
    /// Connects and initializes with `name`.
    async fn initialize(worker: &Worker, token: &str, name: &str) -> Client {
        let mut client = Client {
            stream: worker.connect().await,
            buf: Vec::new(),
            at: 0,
            frames: VecDeque::new(),
            id: 0,
            committed: 0,
            next: 0,
        };
        let request = client
            .request(
                "initialize",
                &format!(
                    "{{\"major\":1,\"client\":{{\"name\":\"{name}\",\"version\":\"1\"}},\
                     \"capabilities\":[],\"token\":\"{token}\"}}"
                ),
            )
            .await;
        let answer = client.answer(request).await;
        client.id = answer["result"]["client"].as_u64().expect("a client id");
        client.committed = answer["result"]["committed"].as_u64().expect("a record");
        client
    }

    /// Sends one request and returns the id it carries.
    async fn request(&mut self, method: &str, params: &str) -> u64 {
        self.next += 1;
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"{method}\",\"params\":{params}}}\n",
            self.next
        );
        self.stream
            .write_all(line.as_bytes())
            .await
            .expect("writes the request");
        self.next
    }

    /// The next line without its newline, or `None` when the worker closed the
    /// connection.
    async fn line(&mut self) -> Option<&[u8]> {
        loop {
            if let Some(len) = self.buf[self.at..].iter().position(|byte| *byte == b'\n') {
                let start = self.at;
                self.at += len + 1;
                return Some(&self.buf[start..start + len]);
            }
            if self.at > CHUNK {
                self.buf.drain(..self.at);
                self.at = 0;
            }
            let mut chunk = [0u8; CHUNK];
            let read = tokio::time::timeout(WAIT, self.stream.read(&mut chunk))
                .await
                .expect("a frame in time")
                .expect("the connection reads");
            if read == 0 {
                assert!(
                    self.buf.len() == self.at,
                    "the worker closed after half a frame"
                );
                return None;
            }
            self.buf.extend_from_slice(&chunk[..read]);
        }
    }

    /// The next frame, or `None` when the worker closed the connection.
    async fn frame(&mut self) -> Option<Value> {
        let line = self.line().await?;
        Some(serde_json::from_slice(line).expect("a frame is JSON"))
    }

    /// The frames up to the first one `stop` holds for, that one last.
    async fn until(&mut self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut taken = Vec::new();
        loop {
            if let Some(at) = self.frames.iter().position(&stop) {
                taken.extend(self.frames.drain(..=at));
                return taken;
            }
            match self.frame().await {
                Some(frame) => self.frames.push_back(frame),
                None => panic!("the worker closed the connection; read {taken:?}"),
            }
        }
    }

    /// The answer of `request`, which keeps the frames read before it.
    async fn answer(&mut self, request: u64) -> Value {
        let mut frames = VecDeque::from(
            self.until(|frame| frame["id"].as_u64() == Some(request))
                .await,
        );
        let answer = frames.pop_back().expect("an answer");
        while let Some(frame) = frames.pop_back() {
            self.frames.push_front(frame);
        }
        answer
    }

    /// Attaches from `from`, and returns the frames the answer came after.
    async fn attach(&mut self, from: u64) -> Vec<Value> {
        let request = self
            .request("attach", &format!("{{\"from\":{from}}}"))
            .await;
        let mut frames = VecDeque::from(
            self.until(|frame| frame["id"].as_u64() == Some(request))
                .await,
        );
        let answer = frames.pop_back().expect("the attach answer");
        assert!(
            answer["result"]["committed"].is_u64(),
            "the answer names the committed record: {answer}"
        );
        frames.into()
    }
}

/// Whether `frame` is the event of `kind`.
fn is_event(frame: &Value, kind: &str) -> bool {
    frame["method"] == "event" && frame["params"]["type"] == kind
}

/// The frames of `frames` that are the notice of `method`.
fn notices<'a>(frames: &'a [Value], method: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|frame| frame["method"] == method)
        .collect()
}

/// A plugin that registers the command `deploy`.
const DEPLOY: &str = r#"
export default function (r) {
  r.registerCommand("deploy", {
    description: "ship it",
    run: () => "shipped",
  });
}
"#;

#[tokio::test]
async fn worker_sends_plugin_commands_on_attach() {
    let home = Home::new();
    home.plugin("deployer", DEPLOY).await;
    let state = state();
    let fake = home.script(r#"{"replies": [{"text": "done"}]}"#).await;
    let worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;
    let frames = client
        .until(|frame| frame["method"] == "plugin_commands")
        .await;
    let frame = notices(&frames, "plugin_commands")
        .pop()
        .expect("a commands frame");
    let commands = frame["params"]["commands"]
        .as_array()
        .unwrap_or_else(|| panic!("the commands are a list: {frame}"));
    assert!(
        commands
            .iter()
            .any(|command| command["name"] == "deploy" && command["description"] == "ship it"),
        "no deploy command in {frame}"
    );
}

#[tokio::test]
async fn worker_runs_a_command_and_notes_its_text_to_every_client() {
    let home = Home::new();
    home.plugin(
        "echoer",
        r#"
export default function (r) {
  r.registerCommand("echo", {
    description: "Echoes its arguments.",
    run: (args) => `got: ${args}`,
  });
}
"#,
    )
    .await;
    let state = state();
    let fake = home.script(r#"{"replies": [{"text": "done"}]}"#).await;
    let worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut a = Client::initialize(&worker, &token, "a").await;
    let mut b = Client::initialize(&worker, &token, "b").await;
    a.attach(a.committed).await;
    b.attach(b.committed).await;

    let sent = a
        .request("command", r#"{"name":"echo","args":"a b  c"}"#)
        .await;
    let answer = a.answer(sent).await;
    assert!(
        answer["result"].is_object(),
        "the command answers an empty object: {answer}"
    );
    let from_a = a.until(|frame| frame["method"] == "note").await;
    let from_b = b.until(|frame| frame["method"] == "note").await;
    for frames in [&from_a, &from_b] {
        let note = notices(frames, "note").pop().expect("a note frame");
        assert_eq!(note["params"]["text"], "got: a b  c", "{note}");
    }
}

#[tokio::test]
async fn worker_command_send_starts_a_turn() {
    let home = Home::new();
    home.plugin(
        "sender",
        r#"
export default function (r) {
  r.registerCommand("tests", {
    description: "Asks for the tests to run.",
    run: () => { r.send("run the tests"); },
  });
}
"#,
    )
    .await;
    let state = state();
    let fake = home.script(r#"{"replies": [{"text": "done"}]}"#).await;
    let worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;

    let sent = client.request("command", r#"{"name":"tests"}"#).await;
    let answer = client.answer(sent).await;
    assert!(answer["result"].is_object(), "{answer}");
    let frames = client.until(|frame| is_event(frame, "turn_end")).await;
    let user: Vec<&Value> = frames
        .iter()
        .filter(|frame| is_event(frame, "user_message"))
        .collect();
    assert_eq!(user.len(), 1, "one message, one event: {frames:?}");
    assert_eq!(user[0]["params"]["text"], "run the tests", "{frames:?}");
}

#[tokio::test]
async fn worker_sends_panels_to_a_late_client() {
    let home = Home::new();
    home.plugin(
        "builder",
        r#"
export default function (r) {
  const panel = r.ui.panel({ id: "build", title: "Build" });
  panel.set(["ok"]);
}
"#,
    )
    .await;
    let state = state();
    let fake = home.script(r#"{"replies": [{"text": "done"}]}"#).await;
    let worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;
    let frames = client
        .until(|frame| frame["method"] == "plugin_panel")
        .await;
    let panel = notices(&frames, "plugin_panel")
        .pop()
        .expect("a panel frame");
    assert_eq!(panel["params"]["id"], "builder/build", "{panel}");
    assert_eq!(panel["params"]["title"], "Build", "{panel}");
    assert_eq!(panel["params"]["lines"][0], "ok", "{panel}");
}

#[tokio::test]
async fn worker_throttles_a_panel_burst() {
    let home = Home::new();
    home.plugin(
        "burst",
        r#"
let panel;
export default function (r) {
  panel = r.ui.panel({ id: "n", title: "N" });
  r.registerCommand("burst", {
    description: "Sets the panel a thousand times.",
    run() { for (let i = 1; i <= 1000; i += 1) panel.set([String(i)]); },
  });
}
"#,
    )
    .await;
    let state = state();
    let fake = home.script(r#"{"replies": [{"text": "done"}]}"#).await;
    let worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;

    let sent = client.request("command", r#"{"name":"burst"}"#).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let answer = client.answer(sent).await;
    assert!(answer["result"].is_object(), "{answer}");
    let mut seen: Vec<Value> = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, client.frame()).await {
        if frame["method"] == "plugin_panel" && frame["params"]["id"] == "burst/n" {
            seen.push(frame);
        }
    }
    assert!(
        seen.len() <= 11,
        "{} panel frames in one second",
        seen.len()
    );
    let last = seen.last().expect("the last lines go out");
    assert_eq!(last["params"]["lines"][0], "1000", "{last}");
}

#[tokio::test]
async fn worker_sends_plugin_notes_to_clients() {
    let home = Home::new();
    home.plugin(
        "slowpoke",
        r#"
export default function (r) {
  r.hook("tool.call", ({ tool }) => {
    if (tool !== "read") return undefined;
    const t = Date.now();
    while (Date.now() - t < 1500) {}
    return undefined;
  });
}
"#,
    )
    .await;
    tokio::fs::create_dir_all(home.work())
        .await
        .expect("the work folder");
    tokio::fs::write(home.work().join("notes.txt"), "the file\n")
        .await
        .expect("the file");
    let state = state();
    let fake = home
        .script(
            r#"{"replies": [
              {"text": "reading", "calls": [{"name": "read", "arguments": {"path": "notes.txt"}}]},
              {"text": "done"}
            ]}"#,
        )
        .await;
    let worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;
    let sent = client
        .request("send", r#"{"text":"read the notes","mode":"queue"}"#)
        .await;
    client.answer(sent).await;
    let frames = client
        .until(|frame| {
            frame["method"] == "note"
                && frame["params"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("slow"))
        })
        .await;
    let note = notices(&frames, "note").pop().expect("a note frame");
    assert!(
        note["params"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("slow")),
        "{note}"
    );
}

/// A note raised while no client is attached has no one to reach, so the
/// worker writes it to its stderr instead of losing it: the run's own end
/// never sees it, since the pump took it first.
#[tokio::test]
async fn worker_prints_a_note_no_client_saw() {
    let home = Home::new();
    home.plugin(
        "slowpoke",
        r#"
export default function (r) {
  r.hook("tool.call", ({ tool }) => {
    if (tool !== "read") return undefined;
    const t = Date.now();
    while (Date.now() - t < 1500) {}
    return undefined;
  });
}
"#,
    )
    .await;
    tokio::fs::create_dir_all(home.work())
        .await
        .expect("the work folder");
    tokio::fs::write(home.work().join("notes.txt"), "the file\n")
        .await
        .expect("the file");
    let state = state();
    let fake = home
        .script(
            r#"{"replies": [
              {"text": "reading", "calls": [{"name": "read", "arguments": {"path": "notes.txt"}}]},
              {"text": "done"}
            ]}"#,
        )
        .await;
    let mut worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;
    // A second client holds the worker up once the first goes, and it attaches
    // to nothing: the note then reaches no one.
    let _watcher = Client::initialize(&worker, &token, "b").await;
    let sent = client
        .request("send", r#"{"text":"read the notes","mode":"queue"}"#)
        .await;
    client.answer(sent).await;
    // The run is open before the client goes, so the tool call it starts runs
    // with no client attached.
    client.until(|frame| is_event(frame, "user_message")).await;
    let leaving = client.request("detach", "{}").await;
    client.answer(leaving).await;
    drop(client);
    let stderr = worker.stderr_until("slow").await;
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("reagent: ") && line.contains("slow")),
        "the note is one stderr line: {stderr:?}"
    );
}

#[tokio::test]
async fn worker_unknown_command_is_an_error() {
    let home = Home::new();
    let state = state();
    let fake = home.script(r#"{"replies": [{"text": "done"}]}"#).await;
    let worker = Worker::start(&home, state.path(), &fake).await;
    let token = worker.entry().await.token;
    let mut client = Client::initialize(&worker, &token, "a").await;
    client.attach(client.committed).await;
    let sent = client.request("command", r#"{"name":"nowhere"}"#).await;
    let answer = client.answer(sent).await;
    let error = &answer["error"];
    assert!(error.is_object(), "the command is refused: {answer}");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(message.contains("nowhere"), "{answer}");
}
