//! The tests of one plugin tool call: the `timeoutMs` it names, the `onCancel`
//! its context carries, and the `tools` a session's start names.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reagent_agent::tool::{Tool, ToolOutput};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{Plugins, SessionOp, SessionReply};

/// The longest a test waits for a call that must not hang, so a missing
/// feature fails a test instead of holding it for the budget of a tool.
const SHORT: Duration = Duration::from_secs(5);

/// Writes one plugin folder: its `package.json` and one file per pair.
async fn plugin(root: &Path, folder: &str, package: &str, files: &[(&str, &str)]) {
    let dir = root.join(folder);
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, body).await.unwrap();
    }
    tokio::fs::write(
        dir.join("package.json"),
        package.to_string().replace("__NAME__", folder),
    )
    .await
    .unwrap();
}

/// The `package.json` of a plugin whose entry is `index.js`.
fn manifest() -> &'static str {
    r#"{"name": "__NAME__", "version": "1.0.0", "type": "module", "reagent": {"entry": "index.js"}}"#
}

/// The `package.json` of a plugin that may start sessions.
fn sessions_manifest() -> &'static str {
    r#"{"name": "__NAME__", "version": "1.0.0", "type": "module", "reagent": {"entry": "index.js", "permissions": {"sessions": true}}}"#
}

/// A temporary plugins root.
fn temp_root() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// A state folder of its own, kept for the life of the process so a load may
/// read it.
fn state() -> PathBuf {
    tempfile::tempdir().unwrap().keep()
}

/// Loads the plugins under `root`, which must have loaded some.
async fn loaded(root: &Path) -> std::sync::Arc<Plugins> {
    let (plugins, notes) = Plugins::load(root, &state(), None).await;
    plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"))
}

/// Runs one plugin tool and answers its result.
async fn run_with(
    plugins: &Plugins,
    name: &str,
    args: &str,
    cancel: &CancellationToken,
) -> ToolOutput {
    let tool = plugins
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == name)
        .unwrap_or_else(|| panic!("no tool named {name}"));
    tool.run(args, cancel).await
}

/// Runs one plugin tool with no cancellation and answers its result.
async fn run(plugins: &Plugins, name: &str, args: &str) -> ToolOutput {
    run_with(plugins, name, args, &CancellationToken::new()).await
}

#[tokio::test]
async fn plugin_tool_timeout_a_promise_that_settles_in_time_answers() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) {
  r.registerTool({
    name: "slow",
    description: "settles late",
    parameters: { type: "object" },
    timeoutMs: 3000,
    run: () => new Promise((resolve) => setTimeout(() => resolve("done"), 2500)),
  });
}"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let out = tokio::time::timeout(SHORT, run(&plugins, "slow", "{}"))
        .await
        .expect("the call answered within the tool's own budget");
    let took = started.elapsed();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(&*out.content, "done");
    assert!(took >= Duration::from_millis(2400), "it took {took:?}");
}

#[tokio::test]
async fn plugin_tool_timeout_a_promise_past_the_tools_own_budget_is_stopped() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) {
  r.registerTool({
    name: "slow",
    description: "settles too late",
    parameters: { type: "object" },
    timeoutMs: 1000,
    run: () => new Promise((resolve) => setTimeout(() => resolve("late"), 2500)),
  });
}"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let out = tokio::time::timeout(SHORT, run(&plugins, "slow", "{}"))
        .await
        .expect("the call stopped within its own budget");
    let took = started.elapsed();
    assert!(out.is_error, "a stopped tool is an error result");
    assert_eq!(&*out.content, "the plugin call was stopped after 1 s");
    assert!(took < Duration::from_millis(2200), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin slow: tool slow stopped after 1 s"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn plugin_tool_timeout_a_value_out_of_range_is_refused() {
    for bad in ["0", "7200001", "1.5", "\"x\""] {
        let root = temp_root();
        let body = format!(
            r#"export default function (r) {{
  r.registerTool({{
    name: "slow",
    description: "bad budget",
    parameters: {{ type: "object" }},
    timeoutMs: {bad},
    run: () => "x",
  }});
}}"#
        );
        plugin(root.path(), "bad", manifest(), &[("index.js", &body)]).await;
        let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
        assert!(plugins.is_none(), "timeoutMs {bad} registered a tool");
        assert!(
            notes
                .iter()
                .any(|note| note.contains("'slow'") && note.contains("timeoutMs")),
            "timeoutMs {bad} gave {notes:?}"
        );
    }
}

#[tokio::test]
async fn plugin_tool_cancel_runs_on_cancel_once() {
    let root = temp_root();
    plugin(
        root.path(),
        "wait",
        manifest(),
        &[(
            "index.js",
            r#"let cancelled = 0;
export default function (r) {
  r.registerTool({
    name: "wait",
    description: "never settles",
    parameters: { type: "object" },
    run: (args, context) => {
      context.onCancel(() => { cancelled += 1; });
      return new Promise(() => {});
    },
  });
  r.registerTool({
    name: "count",
    description: "reads the counter",
    parameters: { type: "object" },
    run: () => String(cancelled),
  });
}"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let call = run_with(&plugins, "wait", "{}", &signal);
    let stop = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    };
    let (out, ()) = tokio::join!(tokio::time::timeout(SHORT, call), stop);
    let out = out.expect("the cancelled call answered at once");
    assert!(out.is_error, "a cancelled call is an error result");
    assert_eq!(&*out.content, "cancelled");
    let out = tokio::time::timeout(SHORT, run(&plugins, "count", "{}"))
        .await
        .expect("the plugin's queue moved on");
    assert_eq!(&*out.content, "1", "onCancel ran once: {}", out.content);
}

#[tokio::test]
async fn plugin_tool_cancel_frees_the_plugins_queue() {
    let root = temp_root();
    plugin(
        root.path(),
        "wait",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) {
  r.registerTool({
    name: "wait",
    description: "never settles",
    parameters: { type: "object" },
    run: () => new Promise(() => {}),
  });
  r.registerTool({
    name: "ping",
    description: "answers",
    parameters: { type: "object" },
    run: () => "pong",
  });
}"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let call = run_with(&plugins, "wait", "{}", &signal);
    let stop = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    };
    let (out, ()) = tokio::join!(tokio::time::timeout(SHORT, call), stop);
    assert_eq!(
        &*out.expect("the cancelled call answered").content,
        "cancelled"
    );
    // The promise of the first call never settles, and the next call of the
    // same plugin must still answer at once.
    let out = tokio::time::timeout(Duration::from_secs(2), run(&plugins, "ping", "{}"))
        .await
        .expect("the plugin waits on no abandoned promise");
    assert_eq!(&*out.content, "pong");
}

#[tokio::test]
async fn plugin_tool_cancel_a_call_that_ended_runs_no_cancel() {
    let root = temp_root();
    plugin(
        root.path(),
        "fine",
        manifest(),
        &[(
            "index.js",
            r#"let cancelled = 0;
export default function (r) {
  r.registerTool({
    name: "fine",
    description: "answers at once",
    parameters: { type: "object" },
    run: (args, context) => {
      context.onCancel(() => { cancelled += 1; });
      return "fine";
    },
  });
  r.registerTool({
    name: "count",
    description: "reads the counter",
    parameters: { type: "object" },
    run: () => String(cancelled),
  });
}"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(&*run(&plugins, "fine", "{}").await.content, "fine");
    let out = tokio::time::timeout(SHORT, run(&plugins, "count", "{}"))
        .await
        .expect("the counter answered");
    assert_eq!(&*out.content, "0", "a call that ended ran no onCancel");
}

#[tokio::test]
async fn plugin_session_tools_a_value_that_is_not_a_string_rejects() {
    let root = temp_root();
    plugin(
        root.path(),
        "hand",
        sessions_manifest(),
        &[(
            "index.js",
            r#"export default function (r) {
  const probe = (tools) => async () => {
    try {
      await r.sessions.start({ task: "x", tools });
      return "no rejection";
    } catch (error) {
      return String(error && error.message ? error.message : error);
    }
  };
  r.registerTool({ name: "number", description: "probes", parameters: { type: "object" }, run: probe(5) });
  r.registerTool({ name: "empty", description: "probes", parameters: { type: "object" }, run: probe("") });
}"#,
        )],
    )
    .await;
    let (talk, _asks) = tokio::sync::mpsc::unbounded_channel();
    let (plugins, notes) = Plugins::load_with_sessions(root.path(), &state(), None, talk).await;
    let plugins = plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"));
    for name in ["number", "empty"] {
        let out = tokio::time::timeout(SHORT, run(&plugins, name, "{}"))
            .await
            .expect("the refusal answered at once");
        assert_eq!(&*out.content, "tools must be a non-empty string", "{name}");
    }
}

#[tokio::test]
async fn plugin_session_tools_go_to_the_host() {
    let root = temp_root();
    plugin(
        root.path(),
        "hand",
        sessions_manifest(),
        &[(
            "index.js",
            r#"export default function (r) {
  r.registerTool({
    name: "hand",
    description: "hands a task to a session",
    parameters: { type: "object", properties: { tools: { type: "string" } } },
    run: (args) => r.sessions.start({ task: "x", tools: String(args.tools) }),
  });
}"#,
        )],
    )
    .await;
    let (talk, mut asks) = tokio::sync::mpsc::unbounded_channel();
    let (plugins, notes) = Plugins::load_with_sessions(root.path(), &state(), None, talk).await;
    let plugins = plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"));
    let call = run(&plugins, "hand", r#"{"tools":"read,grep"}"#);
    let answer = async {
        let ask = asks.recv().await.expect("no ask reached the host");
        match ask.op {
            SessionOp::Start { tools, task, .. } => {
                assert_eq!(tools.as_deref(), Some("read,grep"));
                assert_eq!(task, "x");
            }
            other => panic!("not a start: {other:?}"),
        }
        let _ = ask.reply.send(SessionReply::Started("s1".into()));
    };
    let (out, ()) = tokio::join!(call, answer);
    assert_eq!(&*out.content, "s1", "the answer is the session id");
}

/// The `onCancel` of a call has its own short-job budget: the tool's own
/// deadline, which may be near, does not cut it off, sync or async.
#[tokio::test]
async fn plugin_tool_cancel_outlives_the_tool_deadline() {
    let root = temp_root();
    plugin(
        root.path(),
        "wait",
        manifest(),
        &[(
            "index.js",
            r#"let done = 0;
export default function (r) {
  r.registerTool({
    name: "late",
    description: "cleans up slowly, async",
    parameters: { type: "object" },
    timeoutMs: 1000,
    run: (args, context) => {
      context.onCancel(() => new Promise((resolve) => setTimeout(() => { done += 1; resolve(); }, 1500)));
      return new Promise(() => {});
    },
  });
  r.registerTool({
    name: "busy",
    description: "cleans up slowly, sync",
    parameters: { type: "object" },
    timeoutMs: 1000,
    run: (args, context) => {
      context.onCancel(() => { const t = Date.now(); while (Date.now() - t < 1200) {} done += 1; });
      return new Promise(() => {});
    },
  });
  r.registerTool({
    name: "count",
    description: "reads the counter",
    parameters: { type: "object" },
    run: () => String(done),
  });
}"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    for (tool, want) in [("late", "1"), ("busy", "2")] {
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let call = run_with(&plugins, tool, "{}", &signal);
        let stop = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        };
        let (out, ()) = tokio::join!(tokio::time::timeout(SHORT, call), stop);
        assert_eq!(
            &*out.expect("the cancelled call answered").content,
            "cancelled"
        );
        let out = tokio::time::timeout(SHORT, run(&plugins, "count", "{}"))
            .await
            .expect("the plugin's queue moved on");
        assert_eq!(&*out.content, want, "{tool}: onCancel finished its work");
    }
}

/// A budget with a fraction of a second is named as it is, not cut down.
#[tokio::test]
async fn plugin_tool_timeout_note_names_fractions() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) {
  r.registerTool({
    name: "slow",
    description: "never settles",
    parameters: { type: "object" },
    timeoutMs: 1500,
    run: () => new Promise(() => {}),
  });
}"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let out = tokio::time::timeout(SHORT, run(&plugins, "slow", "{}"))
        .await
        .expect("the call was stopped");
    assert!(out.is_error);
    assert!(out.content.contains("after 1.5 s"), "{}", out.content);
}
