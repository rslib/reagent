//! The `task` plugin this repository ships: the helper session it starts, what
//! that helper may do, the answer it hands back, a prompt it refuses, the
//! provider and model it names, a cancel of the run that asked, and a start
//! that fails.
//!
//! The parent run is a `-p` run of the binary, with the plugin of
//! `plugins/task` copied into a temporary config folder, and every provider is
//! a `fake:` script, so no test calls an API.

use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// The plugin of this repository.
const PLUGIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugins/task");

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
                .prefix("rt")
                .tempdir()
                .expect("a temp folder"),
            state: tempfile::Builder::new()
                .prefix("rt")
                .tempdir_in("/tmp")
                .expect("a short state folder"),
        }
    }

    /// The folder the runs work in, made by the first call.
    async fn work(&self) -> PathBuf {
        let work = self.root.path().join("work");
        tokio::fs::create_dir_all(&work)
            .await
            .expect("the work folder");
        work
    }

    /// The plugins folder of the home's config: `<config dir>/reagent/plugins`.
    fn plugins(&self) -> PathBuf {
        self.root
            .path()
            .join("config")
            .join("reagent")
            .join("plugins")
    }

    /// Copies the plugin of this repository into the home's plugins folder.
    async fn install_task(&self) {
        let dir = self.plugins().join("task");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the plugin folder");
        for name in ["package.json", "index.js"] {
            let from = PathBuf::from(PLUGIN).join(name);
            let to = dir.join(name);
            tokio::fs::copy(&from, &to)
                .await
                .unwrap_or_else(|error| panic!("the plugin file {}: {error}", from.display()));
        }
    }

    /// Writes a fake provider script and returns its `--provider` value.
    async fn script(&self, name: &str, body: &str) -> String {
        let path = self.root.path().join(name);
        tokio::fs::write(&path, body).await.expect("the script");
        format!("fake:{}", path.display())
    }

    /// The command of one run, in the work folder, with the home's config and
    /// state.
    async fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let work = self.work().await;
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
        command
    }

    /// Runs the binary with `env` set on top of the home's own.
    async fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env)
            .await
            .output()
            .await
            .expect("the binary runs")
    }

    /// Runs the binary with no extra environment.
    async fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[]).await
    }

    /// The journal of the session `id`, one parsed line each.
    async fn records(&self, id: &str) -> Vec<Value> {
        let output = self.run(&["session", "cat", id]).await;
        assert!(
            output.status.success(),
            "session cat {id}: exit {} stderr {}",
            output.status,
            text(&output.stderr)
        );
        text(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    /// The ids of the sessions under the state folder, oldest first.
    async fn sessions(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let root = self.state.path().join("reagent").join("sessions");
        let Ok(mut folders) = tokio::fs::read_dir(&root).await else {
            return ids;
        };
        while let Ok(Some(folder)) = folders.next_entry().await {
            let Ok(mut sessions) = tokio::fs::read_dir(folder.path()).await else {
                continue;
            };
            while let Ok(Some(session)) = sessions.next_entry().await {
                let Ok(kind) = session.file_type().await else {
                    continue;
                };
                if kind.is_dir() {
                    ids.push(session.file_name().to_string_lossy().into_owned());
                }
            }
        }
        ids.sort();
        ids
    }

    /// The one session that is not `parent`, which is the helper the parent
    /// started, and its journal.
    async fn helper(&self, parent: &str) -> (String, Vec<Value>) {
        let mut others = self
            .sessions()
            .await
            .into_iter()
            .filter(|id| id != parent)
            .collect::<Vec<_>>();
        assert_eq!(others.len(), 1, "the sessions: {others:?}");
        let id = others.pop().expect("the helper");
        let records = self.records(&id).await;
        (id, records)
    }
}

/// The stdout of a run, as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The session the run that printed `output` started.
fn session_of(output: &Output) -> String {
    text(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["type"] == "start")
        .and_then(|line| line["session"].as_str().map(str::to_owned))
        .expect("the session of the run")
}

/// Every tool result of a journal, as `(text, is_error)`.
fn tool_results(records: &[Value]) -> Vec<(String, bool)> {
    records
        .iter()
        .filter(|line| line["role"] == "tool_result")
        .filter_map(|line| {
            let content = line["content"].as_str()?.to_owned();
            Some((content, line["is_error"] == true))
        })
        .collect()
}

/// A script that calls `task` with `arguments`, then ends the run.
fn task_script(arguments: &str) -> String {
    format!(
        r#"{{"replies": [
          {{"text": "calling", "calls": [{{"name": "task", "arguments": {arguments}}}]}},
          {{"text": "done"}}
        ]}}"#
    )
}

/// The reply of a helper: one turn that says `text` and ends.
fn say_script(text: &str) -> String {
    format!(r#"{{"replies": [{{"text": "{text}"}}]}}"#)
}

/// Runs one parent whose script asks for `prompt`, on the given environment,
/// and answers the run's output.
async fn ask(home: &Home, prompt: &str, env: &[(&str, &str)]) -> Output {
    home.install_task().await;
    let parent = home
        .script(
            "parent.json",
            &task_script(&format!(r#"{{"prompt": "{prompt}"}}"#)),
        )
        .await;
    home.run_with(
        &[
            "--json",
            "--no-serve",
            "--provider",
            &parent,
            "-p",
            "ask the helper",
        ],
        env,
    )
    .await
}

/// A helper that answers with a known text ends its run with `end_turn`, and
/// its text comes back through the `task` call of the run that asked.
#[tokio::test]
async fn task_plugin_returns_the_helper_answer() {
    let home = Home::new();
    home.install_task().await;
    let helper = home
        .script("helper.json", &say_script("child found it"))
        .await;
    let output = ask(
        &home,
        "find the note",
        &[("REAGENT_TASK_PROVIDER", &helper)],
    )
    .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let records = home.records(&session_of(&output)).await;
    let results = tool_results(&records);
    assert_eq!(results.len(), 1, "the tool results: {results:?}");
    assert!(!results[0].1, "the task call failed: {results:?}");
    assert_eq!(results[0].0, "child found it");
}

/// The helper reads a file and is refused a write, because its task names the
/// three read tools and no other one.
#[tokio::test]
async fn task_plugin_helper_can_only_read() {
    let home = Home::new();
    home.install_task().await;
    let work = home.work().await;
    tokio::fs::write(work.join("note.txt"), "the note says 42")
        .await
        .expect("the note");
    let helper = home
        .script(
            "helper.json",
            r#"{"replies": [
              {"text": "reading", "calls": [{"name": "read", "arguments": {"path": "note.txt"}}]},
              {"text": "writing", "calls": [{"name": "write", "arguments": {"path": "out.txt", "content": "x"}}]},
              {"text": "the note says 42"}
            ]}"#,
        )
        .await;
    let output = ask(
        &home,
        "read the note",
        &[("REAGENT_TASK_PROVIDER", &helper)],
    )
    .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let (id, records) = home.helper(&session_of(&output)).await;
    let results = tool_results(&records);
    let read = results
        .iter()
        .find(|(content, _)| content.contains("the note says 42"))
        .unwrap_or_else(|| panic!("no read result in the journal of {id}: {results:?}"));
    assert!(!read.1, "the read failed: {read:?}");
    let refused = results
        .iter()
        .find(|(content, _)| content.contains("unknown tool `write`"))
        .unwrap_or_else(|| panic!("no refused write in the journal of {id}: {results:?}"));
    assert!(refused.1, "the write was not an error: {refused:?}");
    assert!(!work.join("out.txt").exists(), "the helper wrote a file");
    // The helper is told which three tools it has.
    let first = records
        .iter()
        .find(|line| line["role"] == "user")
        .unwrap_or_else(|| panic!("no user message in the journal of {id}"));
    let said = serde_json::to_string(first).expect("the user message");
    assert!(
        said.contains("read, find and grep"),
        "the task of the helper: {said}"
    );
}

/// A prompt that is blank is an error result, and no helper starts.
#[tokio::test]
async fn task_plugin_needs_a_prompt() {
    let home = Home::new();
    home.install_task().await;
    let helper = home
        .script("helper.json", &say_script("child found it"))
        .await;
    let output = ask(&home, "   ", &[("REAGENT_TASK_PROVIDER", &helper)]).await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let parent = session_of(&output);
    let records = home.records(&parent).await;
    let results = tool_results(&records);
    assert_eq!(results.len(), 1, "the tool results: {results:?}");
    assert!(results[0].1, "the task call did not fail: {results:?}");
    assert_eq!(results[0].0, "task needs a prompt");
    assert_eq!(
        home.sessions().await,
        vec![parent],
        "a helper started for a blank prompt"
    );
}

/// The provider and the model the two variables name are the ones the helper
/// runs on.
#[tokio::test]
async fn task_plugin_runs_the_helper_on_its_own_provider_and_model() {
    let home = Home::new();
    home.install_task().await;
    let helper = home.script("helper.json", &say_script("found it")).await;
    let output = ask(
        &home,
        "find the note",
        &[
            ("REAGENT_TASK_PROVIDER", &helper),
            ("REAGENT_TASK_MODEL", "helper-model"),
        ],
    )
    .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let (id, records) = home.helper(&session_of(&output)).await;
    let header = records
        .iter()
        .find(|line| line["type"] == "header")
        .unwrap_or_else(|| panic!("no header in the journal of {id}"));
    assert_eq!(header["model"], "helper-model", "the header: {header}");
    // The helper ran the script of `REAGENT_TASK_PROVIDER`: the script of the
    // run would call `task` again, and the helper called no tool at all.
    assert!(
        tool_results(&records).is_empty(),
        "the helper ran a tool: {:?}",
        tool_results(&records)
    );
}

/// A start that cannot happen is an error result of the `task` call, and the
/// run that asked goes on.
#[tokio::test]
async fn task_plugin_answers_an_error_when_the_helper_does_not_start() {
    let home = Home::new();
    home.install_task().await;
    let output = ask(
        &home,
        "find the note",
        &[("REAGENT_TASK_PROVIDER", "fake:/no/such/replies.json")],
    )
    .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let records = home.records(&session_of(&output)).await;
    let results = tool_results(&records);
    assert_eq!(results.len(), 1, "the tool results: {results:?}");
    assert!(results[0].1, "the task call did not fail: {results:?}");
    assert!(
        results[0].0.contains("task could not start a helper"),
        "the tool results: {results:?}"
    );
    assert!(
        results[0].0.contains("no/such/replies.json"),
        "the tool results: {results:?}"
    );
}

/// A cancel of the run that asked stops the helper too: the helper does not
/// keep running the turns its script still has.
#[tokio::test]
async fn task_plugin_cancel_stops_the_helper() {
    let home = Home::new();
    home.install_task().await;
    let work = home.work().await;
    tokio::fs::write(work.join("note.txt"), "the note says 42")
        .await
        .expect("the note");
    // A helper whose run is long: one turn for each reply, and each turn reads
    // the note again.
    let turns = 5000;
    let one = r#"{"text": "", "calls": [{"name": "read", "arguments": {"path": "note.txt"}}]}"#;
    let mut replies = vec![one.to_string(); turns];
    replies.push(r#"{"text": "all done"}"#.to_string());
    let helper = home
        .script(
            "helper.json",
            &format!(r#"{{"replies": [{}]}}"#, replies.join(",")),
        )
        .await;
    let parent = home
        .script(
            "parent.json",
            &task_script(r#"{"prompt": "read the note"}"#),
        )
        .await;
    let mut run = home
        .command(
            &[
                "--json",
                "--no-serve",
                "--provider",
                &parent,
                "-p",
                "ask the helper",
            ],
            &[("REAGENT_TASK_PROVIDER", &helper)],
        )
        .await;
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let parent_run = run.spawn().expect("the parent run starts");
    let pid = parent_run.id().expect("the parent has a pid");
    // The helper is the session the parent started, so the newest of the two.
    let mut ids = Vec::new();
    for _ in 0..600 {
        ids = home.sessions().await;
        if ids.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ids.len() >= 2, "the sessions: {ids:?}");
    let helper_id = ids.last().expect("the helper").clone();
    // Its run is going once it has tool results of its own in its journal.
    let mut ran = 0;
    let mut seen = String::new();
    for _ in 0..600 {
        let cat = home.run(&["session", "cat", &helper_id]).await;
        let lines = text(&cat.stdout);
        ran = lines.matches(r#""role":"tool_result""#).count();
        if ran >= 3 {
            break;
        }
        seen = format!(
            "exit {} stdout {} stderr {}",
            cat.status,
            lines,
            text(&cat.stderr)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        ran >= 3,
        "the helper {helper_id} ran {ran} turns before the cancel: {seen}"
    );
    let kill = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .await
        .expect("kill runs");
    assert!(kill.success(), "kill -INT {pid} failed");
    let output = parent_run.wait_with_output().await.expect("the run ends");
    // The helper's turn ends at the cancel, not when its script runs out.
    let mut stopped = String::new();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let cat = home.run(&["session", "cat", &helper_id]).await;
        let lines = text(&cat.stdout);
        if lines == stopped {
            break;
        }
        stopped = lines;
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let later = text(&home.run(&["session", "cat", &helper_id]).await.stdout);
    assert_eq!(
        stopped, later,
        "the helper kept running after the cancel of the run that asked"
    );
    let turns_ran = later.matches(r#""role":"tool_result""#).count();
    assert!(
        turns_ran < 200,
        "the helper ran {turns_ran} of its {turns} turns, so it was not stopped: exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    // The parent run ended cancelled, with the code a Ctrl+C gives.
    assert_eq!(
        output.status.code(),
        Some(130),
        "the parent run: stderr {}",
        text(&output.stderr)
    );
    // The helper never reached the last reply of its script.
    assert!(
        !later.contains("all done"),
        "the helper reached the end of its script"
    );
}
