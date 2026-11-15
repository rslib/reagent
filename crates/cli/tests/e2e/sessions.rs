//! The sessions a plugin starts: a tool that delegates to a child worker and
//! waits for its text, the events the child sends, a cancel, and the limit.
//!
//! The parent run is a `-p` run of the binary with a plugin whose tool starts a
//! session, and every provider is a `fake:` script, so no test calls an API.

use std::path::PathBuf;
use std::process::Output;

use serde_json::Value;
use tokio::process::Command;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// A temporary home with a work folder, a config folder and a state folder.
///
/// The state folder sits in a short one, so the socket path of a worker of a
/// session fits the length the platform caps it at.
struct Home {
    root: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            root: tempfile::Builder::new()
                .prefix("rs")
                .tempdir()
                .expect("a temp folder"),
            state: tempfile::Builder::new()
                .prefix("rs")
                .tempdir_in("/tmp")
                .expect("a short state folder"),
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

    /// Writes one plugin folder whose manifest grants `sessions`.
    async fn plugin(&self, folder: &str, entry: &str) {
        let dir = self.plugins().join(folder);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the plugin folder");
        tokio::fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"name": "{folder}", "reagent": {{"entry": "index.js", "permissions": {{"sessions": true}}}}}}"#
            ),
        )
        .await
        .expect("the manifest");
        tokio::fs::write(dir.join("index.js"), entry)
            .await
            .expect("the entry");
    }

    /// Writes one plugin folder whose manifest grants `sessions` and the
    /// `tool.call` hook, which a guard plugin in a child still needs.
    async fn plugin_guarded(&self, folder: &str, entry: &str) {
        let dir = self.plugins().join(folder);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the plugin folder");
        tokio::fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"name": "{folder}", "reagent": {{"entry": "index.js", "permissions": {{"sessions": true, "hooks": ["tool.call"]}}}}}}"#
            ),
        )
        .await
        .expect("the manifest");
        tokio::fs::write(dir.join("index.js"), entry)
            .await
            .expect("the entry");
    }

    /// Writes a plugin folder whose manifest grants nothing.
    async fn plugin_without(&self, folder: &str, entry: &str) {
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

    /// Writes a fake provider script and returns its `--provider` value.
    async fn script(&self, name: &str, body: &str) -> String {
        let path = self.root.path().join(name);
        tokio::fs::write(&path, body).await.expect("the script");
        format!("fake:{}", path.display())
    }

    /// Runs the binary in the work folder, with the home's config and state.
    async fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[]).await
    }

    /// Runs the binary with `env` set on top of the home's own.
    async fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let work = self.work();
        tokio::fs::create_dir_all(&work)
            .await
            .expect("the work folder");
        let mut command = Command::new(BIN);
        command
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.state.path())
            // Never fetch the real models.dev.
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            .env_remove("REAGENT_PROFILE")
            .args(args)
            .current_dir(&work);
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().await.expect("the binary runs")
    }

    /// The tool results the session `id` recorded, in order.
    async fn tool_results(&self, id: &str) -> Vec<String> {
        let output = self.run(&["session", "cat", id]).await;
        assert!(
            output.status.success(),
            "exit {} stderr {}",
            output.status,
            text(&output.stderr)
        );
        text(&output.stdout)
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
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The text of every tool result of the run whose session `output` starts, read
/// from its journal: the `--json` line of a tool result carries no text.
async fn tool_results(home: &Home, output: &Output) -> String {
    let id = lines(output)
        .iter()
        .find(|line| line["type"] == "start")
        .and_then(|line| line["session"].as_str().map(str::to_owned))
        .expect("the session of the run");
    home.tool_results(&id).await.join(" ")
}

/// A script that calls `name` with `arguments`, then ends the run.
fn call_script(name: &str, arguments: &str) -> String {
    format!(
        r#"{{"replies": [
          {{"text": "calling", "calls": [{{"name": "{name}", "arguments": {arguments}}}]}},
          {{"text": "done"}}
        ]}}"#
    )
}

/// The reply of a child worker: one turn that says `text` and ends.
fn say_script(text: &str) -> String {
    format!(r#"{{"replies": [{{"text": "{text}"}}]}}"#)
}

/// A plugin whose tool `delegate` starts a session, waits for its result and
/// answers with its text.
///
/// `provider` is the provider the session starts on, which is the script of the
/// child: a session that inherits the provider of this run would read this
/// run's replies and call this tool again.
const DELEGATE: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "delegate",
    description: "Hands a task to a session and answers what it said.",
    parameters: {
      type: "object",
      properties: {
        prompt: { type: "string" },
        provider: { type: "string" },
      },
      required: ["prompt"],
    },
    run: async (args) => {
      const id = await reagent.sessions.start({
        task: String(args.prompt),
        provider: String(args.provider),
      });
      const { reason, text } = await reagent.sessions.result(id);
      return `${text}`;
    },
  });
}
"#;

/// A child worker that answers with a known text ends its run with `end_turn`,
/// and its text comes back through the tool that started it.
#[tokio::test]
async fn plugin_session_a_delegated_task_answers_its_text() {
    let home = Home::new();
    home.plugin("boss", DELEGATE).await;
    let child = home.script("child.json", &say_script("child done")).await;
    let parent = home
        .script(
            "parent.json",
            &call_script(
                "delegate",
                &format!(r#"{{"prompt": "say it", "provider": "{child}"}}"#),
            ),
        )
        .await;
    // A session starts on the provider its start names: without it the child
    // would read the replies of the parent, which call the tool again.
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "ask the child",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("child done"), "the tool results: {saw}");
    let _ = &child;
}

/// The listeners of the plugin that started a session see its frames in order,
/// and a plugin without the permission has no `reagent.sessions` at all.
#[tokio::test]
async fn plugin_session_only_the_owner_sees_the_frames() {
    let home = Home::new();
    home.plugin("worker_owner", WATCHER).await;
    home.plugin_without("quiet", SHOWS_NOTHING).await;
    let child = home
        .script(
            "child.json",
            r#"{"replies": [{"text": "first"}, {"text": "seen"}]}"#,
        )
        .await;
    let parent = home
        .script(
            "parent.json",
            &format!(
                r#"{{"replies": [
                  {{"text": "watching", "calls": [{{"name": "watch", "arguments": {{"provider": "{child}"}}}}]}},
                  {{"text": "waiting", "calls": [{{"name": "wait", "arguments": {{}}}}]}},
                  {{"text": "reading", "calls": [{{"name": "seen", "arguments": {{}}}}]}},
                  {{"text": "done"}}
                ]}}"#
            ),
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "watch the child",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("turn_start"), "the events: {saw}");
    assert!(saw.contains("text_delta"), "the events: {saw}");
    assert!(saw.contains("run_end"), "the events: {saw}");
    let _ = &child;
}

/// A plugin that starts a child whose worker answers, and reports each event
/// its listener saw.
const WATCHER: &str = r#"
const seen = [];
let watched = null;
export default function (reagent) {
  reagent.registerTool({
    name: "watch",
    description: "Watches a session it starts and answers what it saw.",
    parameters: {
      type: "object",
      properties: { provider: { type: "string" } },
    },
    run: async (args) => {
      const id = await reagent.sessions.start({
        task: "say it",
        provider: String(args.provider),
      });
      // The run of the task ends on its own, and its end is read here so that
      // the next read waits for the run the queued message starts: a listener
      // sees the frames that arrive after it is put on the session.
      await reagent.sessions.result(id);
      reagent.sessions.on(id, (e) => seen.push(e.type));
      await reagent.sessions.send(id, "again");
      watched = id;
      return "watching";
    },
  });
  reagent.registerTool({
    name: "wait",
    description: "Waits for the run of the session to end.",
    parameters: { type: "object", properties: {} },
    run: async () => {
      const { text } = await reagent.sessions.result(watched);
      return `ended ${text}`;
    },
  });
  reagent.registerTool({
    name: "seen",
    description: "Answers the events its listener saw.",
    parameters: { type: "object", properties: {} },
    run: () => seen.join(","),
  });
}
"#;

/// A plugin without the session permission says what `reagent.sessions` is.
const SHOWS_NOTHING: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "probe_sessions",
    description: "Says what reagent.sessions is.",
    parameters: { type: "object", properties: {} },
    run: () => typeof reagent.sessions,
  });
}
"#;

/// The limit: the ninth `start` of one plugin rejects, and the message names
/// the limit.
#[tokio::test]
async fn plugin_session_the_ninth_start_rejects() {
    let home = Home::new();
    home.plugin("many", MANY).await;
    let child = home.script("child.json", &say_script("seen")).await;
    let parent = home
        .script(
            "parent.json",
            &call_script("many", &format!(r#"{{"provider": "{child}"}}"#)),
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "start many",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("limit"), "the tool results: {saw}");
}

/// A plugin that starts nine sessions and answers with what the ninth said.
const MANY: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "many",
    description: "Starts sessions until the limit refuses one.",
    parameters: {
      type: "object",
      properties: { provider: { type: "string" } },
    },
    run: async (args) => {
      const said = [];
      for (let n = 0; n < 9; n += 1) {
        try {
          await reagent.sessions.start({
            task: "say it",
            provider: String(args.provider),
          });
        } catch (error) {
          said.push(String(error));
        }
      }
      return said.join(",") || "started nine";
    },
  });
}
"#;

/// A plugin that starts a child, cancels it and answers why its run ended.
#[tokio::test]
async fn plugin_session_a_cancel_ends_the_child() {
    let home = Home::new();
    home.plugin("stopper", STOPPER).await;
    // The child's first reply calls a bash command that sleeps, so its turn is
    // still running when the cancel comes.
    let slow = serde_json::json!({
        "replies": [
            {"text": "first"},
            {"calls": [{"name": "bash", "arguments": {"command": "sleep 30"}}]},
            {"text": "done"}
        ]
    })
    .to_string();
    let slow = home.script("child.json", &slow).await;
    let parent = home
        .script(
            "parent.json",
            &call_script("stop", &format!(r#"{{"provider": "{slow}"}}"#)),
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "stop the child",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("cancelled"), "the tool results: {saw}");
}

/// A plugin that starts a session, waits for it to start its turn, cancels it
/// and answers the reason of its run end.
const STOPPER: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "stop",
    description: "Starts a session, cancels it and answers its reason.",
    parameters: {
      type: "object",
      properties: { provider: { type: "string" } },
    },
    run: async (args) => {
      const id = await reagent.sessions.start({
        task: "sleep",
        provider: String(args.provider),
      });
      // The first run ends on its own; the queued message starts the run whose
      // turn calls the command that sleeps.
      await reagent.sessions.result(id);
      await reagent.sessions.send(id, "sleep");
      // The turn of the child is running by now, so the cancel ends it.
      const began = new Promise((resolve) => setTimeout(resolve, 1500));
      await began;
      await reagent.sessions.cancel(id);
      const { reason } = await reagent.sessions.result(id);
      return `reason ${reason}`;
    },
  });
}
"#;

/// A `result` after a `send` waits for the run that the message starts, and not
/// for the end of the run before it.
#[tokio::test]
async fn plugin_session_a_send_answers_its_own_run() {
    let home = Home::new();
    home.plugin("again", TWO_RUNS).await;
    let child = home
        .script(
            "child.json",
            r#"{"replies": [{"text": "first"}, {"text": "second"}]}"#,
        )
        .await;
    let parent = home
        .script(
            "parent.json",
            &call_script("tworuns", &format!(r#"{{"provider": "{child}"}}"#)),
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "ask twice",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("first|second"), "the tool results: {saw}");
}

/// A plugin that reads one run, sends again and reads the run that message
/// starts.
const TWO_RUNS: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "tworuns",
    description: "Reads the run of the task and the run of a queued message.",
    parameters: {
      type: "object",
      properties: { provider: { type: "string" } },
    },
    run: async (args) => {
      const id = await reagent.sessions.start({
        task: "say it",
        provider: String(args.provider),
      });
      const first = await reagent.sessions.result(id);
      await reagent.sessions.send(id, "say it again");
      const second = await reagent.sessions.result(id);
      return `${first.text}|${second.text}`;
    },
  });
}
"#;

/// A run one session deep, which is what a session's worker is, loads its
/// plugins, guard hooks included, and starts no session of its own.
#[tokio::test]
async fn plugin_session_a_session_starts_no_session() {
    let home = Home::new();
    home.plugin_guarded("nested", PROBE_START).await;
    let parent = home
        .script(
            "parent.json",
            r#"{"replies": [
              {"text": "starting", "calls": [{"name": "probe_start", "arguments": {}}]},
              {"text": "shell", "calls": [{"name": "bash", "arguments": {"command": "echo hi"}}]},
              {"text": "done"}
            ]}"#,
        )
        .await;
    let output = home
        .run_with(
            &[
                "--json",
                "--no-serve",
                "--provider",
                &parent,
                "-p",
                "start a session",
            ],
            &[("REAGENT_SESSION_DEPTH", "1")],
        )
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(
        saw.contains("a session cannot start sessions"),
        "the tool results: {saw}"
    );
    // The guard hook of the plugin loaded all the same, and it still refuses
    // the tools it names.
    assert!(
        saw.contains("the gate of the child"),
        "the tool results: {saw}"
    );
}

/// A plugin that tries to start a session and answers what came of it, which is
/// what a worker a session started reads.
const PROBE_START: &str = r#"
export default function (reagent) {
  reagent.hook("tool.call", ({ tool }) =>
    tool === "bash" ? { block: "the gate of the child" } : undefined);
  reagent.registerTool({
    name: "probe_start",
    description: "Tries to start a session and answers what it got.",
    parameters: { type: "object", properties: {} },
    run: async () => {
      try {
        const id = await reagent.sessions.start({ task: "nested" });
        return `started ${id}`;
      } catch (error) {
        return String(error);
      }
    },
  });
}
"#;

/// A worker a session starts is one session deeper than the run that started
/// it, which is what stops a session of a session.
#[tokio::test]
async fn plugin_session_a_child_is_one_deeper() {
    let home = Home::new();
    home.plugin("depth", DELEGATE).await;
    let child = home
        .script(
            "child.json",
            r#"{"replies": [
              {"text": "checking", "calls": [{"name": "bash", "arguments": {"command": "printf %s \"$REAGENT_SESSION_DEPTH\" > depth.txt"}}]},
              {"text": "checked"}
            ]}"#,
        )
        .await;
    let parent = home
        .script(
            "parent.json",
            &call_script(
                "delegate",
                &format!(r#"{{"prompt": "check the depth", "provider": "{child}"}}"#),
            ),
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "ask the child",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let depth = tokio::fs::read_to_string(home.work().join("depth.txt"))
        .await
        .expect("the depth of the child's worker");
    assert_eq!(depth, "1", "the depth the child's worker read");
}

/// A task of `-` would read a message from a stdin the worker does not have, so
/// a start refuses it with a reason a person reads.
#[tokio::test]
async fn plugin_session_a_dash_task_is_refused() {
    let home = Home::new();
    home.plugin("dash", DASH).await;
    let parent = home.script("parent.json", &call_script("dash", "{}")).await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "read stdin",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("stdin"), "the tool results: {saw}");
}

/// A plugin that starts a session on a task of `-` and answers the refusal.
const DASH: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "dash",
    description: "Starts a session whose task is a dash.",
    parameters: { type: "object", properties: {} },
    run: async () => {
      try {
        await reagent.sessions.start({ task: "-" });
        return "started a dash";
      } catch (error) {
        return String(error);
      }
    },
  });
}
"#;

/// Nine starts made at once leave the ninth out: a start that still waits for
/// its worker takes a place of its own.
#[tokio::test]
async fn plugin_session_nine_starts_at_once_leave_the_ninth_out() {
    let home = Home::new();
    home.plugin("parallel", PARALLEL).await;
    let child = home.script("child.json", &say_script("seen")).await;
    let parent = home
        .script(
            "parent.json",
            &call_script("parallel", &format!(r#"{{"provider": "{child}"}}"#)),
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "start nine at once",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("limit"), "the tool results: {saw}");
    assert_eq!(
        saw.matches("limit").count(),
        1,
        "one start of nine was refused: {saw}"
    );
}

/// A plugin that starts nine sessions at once and answers what each said.
const PARALLEL: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "parallel",
    description: "Starts nine sessions at once and answers the refusals.",
    parameters: {
      type: "object",
      properties: { provider: { type: "string" } },
    },
    run: async (args) => {
      const tries = [];
      for (let n = 0; n < 9; n += 1) {
        tries.push(
          reagent.sessions
            .start({ task: "say it", provider: String(args.provider) })
            .then(() => "ok", (error) => String(error)),
        );
      }
      const answers = await Promise.all(tries);
      return answers.filter((answer) => answer !== "ok").join(",") || "started nine";
    },
  });
}
"#;

/// A session whose worker died leaves `list`, and its place is free for the
/// next start, which a session that stays in `list` would take.
#[tokio::test]
async fn plugin_session_a_dead_worker_leaves_the_list() {
    let home = Home::new();
    home.plugin("killer", KILLER).await;
    // The child kills the worker that runs it, in the turn of its own task.
    let child = home
        .script(
            "child.json",
            r#"{"replies": [
              {"text": "dying", "calls": [{"name": "bash", "arguments": {"command": "kill -9 $PPID"}}]}
            ]}"#,
        )
        .await;
    let parent = home
        .script(
            "parent.json",
            &call_script("kill", &format!(r#"{{"provider": "{child}"}}"#)),
        )
        .await;
    let output = home
        .run(&[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "kill the child",
        ])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let saw = tool_results(&home, &output).await;
    assert!(saw.contains("left 0"), "the tool results: {saw}");
    assert!(saw.contains("next started"), "the tool results: {saw}");
}

/// A plugin that starts a session whose worker kills itself, and answers what
/// `list` and the next start say after it went.
const KILLER: &str = r#"
export default function (reagent) {
  reagent.registerTool({
    name: "kill",
    description: "Starts a session that kills its worker and reads list after.",
    parameters: {
      type: "object",
      properties: { provider: { type: "string" } },
    },
    run: async (args) => {
      const provider = String(args.provider);
      const id = await reagent.sessions.start({ task: "die", provider });
      await reagent.sessions.result(id);
      // The worker died, so its task ends and the host forgets it.
      const gone = new Promise((resolve) => setTimeout(resolve, 500));
      await gone;
      const left = await reagent.sessions.list();
      let next = "next failed";
      try {
        await reagent.sessions.start({ task: "live", provider });
        next = "next started";
      } catch (error) {
        next = String(error);
      }
      return `left ${left.length} ${next}`;
    },
  });
}
"#;
