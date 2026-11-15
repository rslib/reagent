//! The `node:child_process` tests: built with the `node` feature only.
//!
//! Every child of a test starts through a stand-in sandbox helper: a `sh`
//! script that writes its own environment to a file, logs the arguments it was
//! given, applies `--env-clear` and `--env NAME=VALUE` as the real helper does,
//! drops the flags up to and including `--`, and `exec`s the rest with that
//! environment. So a test sees exactly what reagent asks the real helper for,
//! without a sandbox of its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reagent_agent::tool::{Tool, ToolOutput};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::super::Plugins;

/// The longest a test waits for a process to show up or go away.
const WAIT: Duration = Duration::from_secs(5);

/// Writes one plugin folder: its `package.json` and one file per pair.
async fn plugin(root: &Path, folder: &str, permissions: &str, files: &[(&str, &str)]) {
    plugin_named(root, folder, folder, permissions, files).await;
}

/// Writes one plugin folder whose `package.json` names it `name`, which is what
/// a plugin is called everywhere but its folder.
async fn plugin_named(
    root: &Path,
    folder: &str,
    name: &str,
    permissions: &str,
    files: &[(&str, &str)],
) {
    let dir = root.join(folder);
    for (file, body) in files {
        let path = dir.join(file);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, body).await.unwrap();
    }
    let package = format!(
        r#"{{"name": "{name}", "version": "1.0.0", "type": "module", "reagent": {{"entry": "index.js", "permissions": {permissions}}}}}"#
    );
    tokio::fs::write(dir.join("package.json"), package)
        .await
        .unwrap();
}

/// The entry of a plugin with one tool named `try`, awaiting `body`.
fn awaited(imports: &str, body: &str) -> String {
    format!(
        r#"{imports}
export default function (reagent) {{
  reagent.registerTool({{
    name: "try",
    description: "Runs the test's code.",
    parameters: {{ type: "object" }},
    async run() {{ {body} }},
  }});
}}
"#
    )
}

/// Writes one plugin with one awaited tool named `try`.
async fn awaited_tool(root: &Path, folder: &str, permissions: &str, imports: &str, body: &str) {
    let source = awaited(imports, body);
    plugin(root, folder, permissions, &[("index.js", &source)]).await;
}

/// The `fs` functions the tests use, so a tool reports what it saw through a
/// file instead of its own result.
const FS: &str = r#"import { readFileSync, writeFileSync } from "fs";"#;

/// A temporary folder for the plugins.
fn temp_root() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// A state folder of its own, kept for the life of the process.
fn state() -> PathBuf {
    tempfile::tempdir().unwrap().keep()
}

/// The file the stand-in helper writes its own environment to, one line per
/// variable, before it parses any argument.
fn helper_env(folder: &Path) -> PathBuf {
    folder.join("helper.env")
}

/// Writes the stand-in sandbox helper into `folder` and answers its path and
/// the log it appends to: a `sh` script that first writes its own environment
/// to [`helper_env`], then logs its arguments one per line, then applies
/// `--env-clear` and `--env NAME=VALUE` the way the real helper does, and
/// `exec`s the program with that environment.
async fn helper(folder: &Path) -> (PathBuf, PathBuf) {
    let log = folder.join("helper.log");
    let path = folder.join("helper.sh");
    let body = format!(
        concat!(
            "#!/bin/sh\n",
            "/usr/bin/env > {env}\n",
            "for arg in \"$@\"; do printf '%s\\n' \"$arg\" >> {log}\ndone\n",
            "clear=0\n",
            "while [ $# -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    --) shift; break ;;\n",
            "    --env-clear) clear=1; shift ;;\n",
            "    --env) printf '%s\\n' \"$2\" >> {pairs}; shift 2 ;;\n",
            "    *) shift ;;\n",
            "  esac\n",
            "done\n",
            "if [ \"$clear\" = 1 ]; then\n",
            "  for name in $(/usr/bin/env | /usr/bin/cut -d= -f1); do unset \"$name\"; done\n",
            "fi\n",
            "if [ -s {pairs} ]; then\n",
            "  while IFS= read -r pair; do export \"$pair\"; done < {pairs}\n",
            "fi\n",
            "exec \"$@\"\n",
        ),
        env = helper_env(folder).display(),
        log = log.display(),
        pairs = folder.join("helper.pairs").display(),
    );
    tokio::fs::write(&path, body).await.unwrap();
    set_executable(&path).await;
    (path, log)
}

/// Makes `path` runnable.
async fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::Permissions::from_mode(0o755);
    tokio::fs::set_permissions(path, mode).await.unwrap();
}

/// Writes a stand-in helper that refuses to start, as the real one does when
/// the sandbox cannot start, and answers its path and log.
async fn refusing_helper(folder: &Path) -> (PathBuf, PathBuf) {
    let log = folder.join("helper.log");
    let path = folder.join("refuse.sh");
    let body = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> {}\ndone\necho 'reagent: sandbox unavailable: no sandbox here' >&2\nexit 125\n",
        log.display()
    );
    tokio::fs::write(&path, body).await.unwrap();
    set_executable(&path).await;
    (path, log)
}

/// The lines the helper logged, one per argument.
#[expect(clippy::disallowed_methods, reason = "test read of a temp dir")]
fn log_lines(log: &Path) -> Vec<String> {
    match std::fs::read_to_string(log) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Loads the plugins under `root` with `session` the folder a relative path
/// resolves against and `sandbox` the helper every child starts through, which
/// must have loaded some.
async fn loaded(
    root: &Path,
    session: &Path,
    sandbox: Option<PathBuf>,
) -> (Arc<Plugins>, Vec<String>) {
    let (plugins, notes) = Plugins::load_with(
        root,
        session,
        &state(),
        sandbox,
        crate::plugin::Role::Run(None),
    )
    .await;
    let plugins = plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"));
    (plugins, notes)
}

/// Loads the plugins under `root`, which must not have loaded.
async fn refused(root: &Path, session: &Path, sandbox: Option<PathBuf>) -> Vec<String> {
    let (plugins, notes) = Plugins::load_with(
        root,
        session,
        &state(),
        sandbox,
        crate::plugin::Role::Run(None),
    )
    .await;
    assert!(plugins.is_none(), "{notes:?}");
    notes
}

/// Runs one plugin tool and answers with its result.
async fn run(plugins: &Plugins, name: &str) -> ToolOutput {
    let tool = plugins
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == name)
        .unwrap_or_else(|| panic!("no plugin tool named {name}"));
    tool.run("{}", &CancellationToken::new()).await
}

/// The text one tool answered with.
async fn text(plugins: &Plugins) -> String {
    run(plugins, "try").await.content.to_string()
}

/// The pid a plugin wrote to `file`.
async fn pid_of(file: &Path) -> u32 {
    tokio::fs::read_to_string(file)
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Whether `pid` is still there, zombie included.
fn alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// Waits until `pid` is gone, and fails when it is still there after [`WAIT`].
async fn gone(pid: u32) {
    let began = Instant::now();
    while began.elapsed() < WAIT {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("process {pid} is still there after {WAIT:?}");
}

#[tokio::test]
async fn child_exec_file_echo() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "echoes",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], (error, stdout) => resolve(`${error} ${stdout}`)));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path.clone())).await;
    assert_eq!(&*text(&plugins).await, "null hi\n");
    let lines = log_lines(&log);
    let dash = lines
        .iter()
        .position(|line| line == "--")
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert_eq!(&lines[..1], ["--sandbox-exec"]);
    assert!(!lines.contains(&"--network".to_string()), "{lines:?}");
    let temp = canonical_temp().await;
    assert!(
        lines[..dash]
            .windows(2)
            .any(|pair| pair[0] == "--write-resolved" && pair[1] == temp),
        "{lines:?}"
    );
    assert_eq!(lines[dash + 1], canonical("/bin/echo").await);
    assert_eq!(&lines[dash + 2..], ["hi".to_string()]);
}

/// The temporary folder every child may write, canonical.
async fn canonical_temp() -> String {
    tokio::fs::canonicalize(std::env::temp_dir())
        .await
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// `path` with every link resolved, as text.
async fn canonical(path: &str) -> String {
    tokio::fs::canonicalize(path)
        .await
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn child_spawn_stdin_cat() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "cats",
        r#"{"run": ["cat"]}"#,
        r#"import { spawn } from "node:child_process";"#,
        r#"
      const child = spawn("cat", []);
      let out = "";
      const closed = new Promise((resolve) => child.on("close", (code) => resolve(code)));
      child.stdout.on("data", (chunk) => { out += chunk.toString(); });
      child.stdin.write("abc");
      child.stdin.end();
      const code = await closed;
      return `${out} ${code}`;
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "abc 0");
}

#[tokio::test]
async fn child_exec_needs_sh() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "runs",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { exec } from "node:child_process";"#,
        r#"return await new Promise((resolve) => exec("echo hi", (error) => resolve(String(error))));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path.clone())).await;
    let answer = text(&plugins).await;
    assert!(answer.contains("sh"), "{answer}");
    assert!(log_lines(&log).is_empty(), "{:?}", log_lines(&log));

    let second = temp_root();
    let (again, _) = helper(session.path()).await;
    awaited_tool(
        second.path(),
        "runs",
        r#"{"run": ["sh"]}"#,
        r#"import { exec } from "node:child_process";"#,
        r#"return await new Promise((resolve) => exec("echo hi", (error, stdout) => resolve(`${error} ${stdout}`)));"#,
    )
    .await;
    let (plugins, notes) = loaded(second.path(), session.path(), Some(again)).await;
    // A plugin that lists a shell can run any program through it, and the load
    // says so.
    assert!(
        notes
            .iter()
            .any(|note| note.contains("runs") && note.contains("shell sh")),
        "{notes:?}"
    );
    assert_eq!(&*text(&plugins).await, "null hi\n");
}

#[tokio::test]
async fn child_exit_code_and_signal() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "exits",
        r#"{"run": ["sh", "sleep"]}"#,
        r#"import { execFile, spawn } from "node:child_process";"#,
        r#"
      const failed = await new Promise((resolve) => {
        execFile("sh", ["-c", "exit 3"], (error, stdout, stderr) => {
          resolve(`code=${error && error.code} stdout=${stdout} stderr=${stderr}`);
        });
      });
      const child = spawn("sleep", ["30"]);
      const signaled = new Promise((resolve) => child.on("close", (code, signal) => resolve(`${code} ${signal}`)));
      child.kill("SIGKILL");
      return `${failed} ${await signaled}`;
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(
        &*text(&plugins).await,
        "code=3 stdout= stderr= null SIGKILL"
    );
}

#[tokio::test]
async fn child_unlisted_refused() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    let victim = session.path().join("kept.txt");
    tokio::fs::write(&victim, "keep").await.unwrap();
    let body = format!(
        r#"
      const refused = [];
      for (const [file, args] of [["/bin/rm", ["-f", {victim:?}]], ["./echo", []], ["rm", []]]) {{
        try {{ spawn(file, args); }} catch (error) {{ refused.push(String(error.message || error)); }}
      }}
      return refused.join(" | ");
    "#,
        victim = victim.to_string_lossy()
    );
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { spawn } from "node:child_process";"#,
        &body,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    assert!(answer.contains("/bin/rm"), "{answer}");
    assert!(answer.contains("./echo"), "{answer}");
    assert!(answer.contains("rm"), "{answer}");
    assert!(victim.exists());
    assert!(log_lines(&log).is_empty(), "{:?}", log_lines(&log));
}

#[tokio::test]
async fn child_no_run_refused() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        "{}",
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], (error) => resolve(String(error))));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    assert!(answer.contains("spawns"), "{answer}");
    assert!(log_lines(&log).is_empty(), "{:?}", log_lines(&log));
}

#[tokio::test]
async fn child_no_sandbox_refused() {
    let root = temp_root();
    let session = temp_root();
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], (error) => resolve(String(error))));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), None).await;
    let answer = text(&plugins).await;
    assert!(answer.contains("sandbox"), "{answer}");
    assert!(answer.contains("/bin/echo"), "{answer}");
}

#[tokio::test]
async fn child_sandbox_unavailable() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = refusing_helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], (error) => resolve(String(error))));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    assert!(answer.contains("no sandbox here"), "{answer}");
    assert!(answer.to_lowercase().contains("sandbox"), "{answer}");
}

#[tokio::test]
async fn child_bad_entry_refuses_plugin() {
    let root = temp_root();
    let session = temp_root();
    awaited_tool(
        root.path(),
        "wants_ghost",
        r#"{"run": ["no-such-program-x"]}"#,
        "",
        "return 1;",
    )
    .await;
    let notes = refused(root.path(), session.path(), None).await;
    assert!(
        notes
            .iter()
            .any(|note| note.contains("no-such-program-x") && note.contains("wants_ghost")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn child_network_flag() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "fetches",
        r#"{"net": ["*"], "run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], () => resolve("done")));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path.clone())).await;
    assert_eq!(&*text(&plugins).await, "done");
    assert!(
        log_lines(&log).contains(&"--network".to_string()),
        "{:?}",
        log_lines(&log)
    );
    tokio::fs::remove_file(&log).await.unwrap();

    let second = temp_root();
    let (again, again_log) = helper(session.path()).await;
    awaited_tool(
        second.path(),
        "fetches",
        r#"{"net": ["127.0.0.1"], "run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], () => resolve("done")));"#,
    )
    .await;
    let (plugins, _) = loaded(second.path(), session.path(), Some(again)).await;
    assert_eq!(&*text(&plugins).await, "done");
    let lines = log_lines(&again_log);
    assert!(!lines.contains(&"--network".to_string()), "{lines:?}");
}

#[tokio::test]
async fn child_write_folders() {
    let root = temp_root();
    let session = temp_root();
    let out = session.path().join("out");
    tokio::fs::create_dir(out.clone()).await.unwrap();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "writes",
        r#"{"fs": {"write": ["./out"]}, "run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], () => resolve("done")));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "done");
    let lines = log_lines(&log);
    assert!(
        lines.windows(2).any(|pair| pair[0] == "--write-resolved"
            && pair[1] == out.canonicalize().unwrap().to_string_lossy()),
        "{lines:?}"
    );
}

#[tokio::test]
async fn child_env_clean() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    // A variable of the test process the plugin's `env` permission does not
    // name: the child may not see it, whatever reagent's own environment holds.
    // None of these is one a shell fills in for itself.
    let kept = ["TMPDIR", "LANG", "TERM", "LOGNAME", "USER"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
        .expect("the test process has no environment");
    let body = format!(
        r#"
      return await new Promise((resolve) => {{
        execFile("/bin/sh", ["-c", {script:?}], {{ env: {{ OTHER_CHILD: "given" }} }}, (error, stdout) => resolve(`${{error}} ${{stdout}}`));
      }});
    "#,
        script = format!("printf '%s|%s|%s' \"$HOME\" \"${kept}\" \"${{OTHER_CHILD}}\"")
    );
    awaited_tool(
        root.path(),
        "envs",
        r#"{"env": ["HOME"], "run": ["/bin/sh"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        &body,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    let fields: Vec<&str> = answer
        .split(' ')
        .nth(1)
        .unwrap_or_default()
        .split('|')
        .collect();
    assert_eq!(fields.len(), 3, "{answer}");
    assert!(fields[0].starts_with('/'), "{answer}");
    assert_eq!(fields[1], "", "{kept} reached the child: {answer}");
    assert_eq!(fields[2], "given", "{answer}");
}

#[tokio::test]
async fn child_bad_options() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo", "sleep"]}"#,
        r#"import { spawn } from "node:child_process";"#,
        r#"
      const refused = [];
      for (const options of [{ stdio: "inherit" }, { detached: true }, { uid: 0 }, { shell: "/bin/zsh" }]) {
        try { spawn("/bin/echo", ["hi"], options); refused.push("no"); }
        catch (error) { refused.push(error instanceof TypeError ? "type" : "other"); }
      }
      return refused.join(",");
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "type,type,type,type");
    assert!(log_lines(&log).is_empty(), "{:?}", log_lines(&log));
}

#[tokio::test]
async fn child_cwd_checked() {
    let root = temp_root();
    let session = temp_root();
    let outside = temp_root();
    let (path, log) = helper(session.path()).await;
    let outside_path = outside.path().to_string_lossy().into_owned();
    awaited_tool(
        root.path(),
        "walks",
        r#"{"run": ["/bin/pwd"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        &format!(
            r#"
      const refused = await new Promise((resolve) => {{
        execFile("/bin/pwd", [], {{ cwd: {outside_path:?} }}, (error) => resolve(String(error && error.message)));
      }});
      const inside = await new Promise((resolve) => {{
        execFile("/bin/pwd", [], (error, stdout) => resolve(`${{error}} ${{stdout}}`));
      }});
      return `${{refused}} | ${{inside}}`;
    "#
        ),
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    let (refused, inside) = answer.split_once(" | ").unwrap_or_default();
    assert!(refused.contains("fs read"), "{answer}");
    assert!(
        inside.contains(&*session.path().canonicalize().unwrap().to_string_lossy()),
        "{answer}"
    );
    let lines = log_lines(&log);
    // One start only: the refused `cwd` never reached the helper.
    assert_eq!(
        lines.iter().filter(|line| *line == "--").count(),
        1,
        "{lines:?}"
    );
    assert!(lines.contains(&canonical("/bin/pwd").await), "{lines:?}");
}

#[tokio::test]
async fn child_limit_16() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "many",
        r#"{"run": ["sleep"]}"#,
        r#"import { spawn } from "node:child_process";"#,
        r#"
      const children = [];
      for (let i = 0; i < 16; i += 1) children.push(spawn("sleep", ["30"]));
      let refused = "started";
      try { spawn("sleep", ["30"]); } catch (error) { refused = String(error.message || error); }
      const first = children[0];
      const exited = new Promise((resolve) => first.on("close", () => resolve(true)));
      first.kill("SIGKILL");
      await exited;
      const again = spawn("sleep", ["1"]);
      for (const child of children) child.kill("SIGKILL");
      again.kill("SIGKILL");
      return refused;
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    assert!(answer.contains("16"), "{answer}");
}

/// A program that leaves a process of its own session behind, holding its
/// stdout and stderr, prints that process's pid and exits.
const ESCAPE: &str = "import os, time\nr, w = os.pipe()\npid = os.fork()\nif pid == 0:\n    os.setsid()\n    os.write(w, b'x')\n    time.sleep(60)\nelse:\n    os.read(r, 1)\n    print(pid, flush=True)\n";

#[tokio::test]
async fn child_limit_counts_held_pipes() {
    let found = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("python3").is_file()));
    if !found {
        eprintln!("no python3 on PATH: skipped");
        return;
    }
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "escapes",
        r#"{"run": ["python3"]}"#,
        r#"import { spawn } from "node:child_process";"#,
        &format!(
            r#"
      const pids = [];
      for (let i = 0; i < 16; i += 1) {{
        const child = spawn("python3", ["-c", {ESCAPE:?}]);
        await new Promise((resolve) => {{
          let printed = false;
          let exited = false;
          const done = () => {{ if (printed && exited) resolve(); }};
          child.stdout.on("data", (chunk) => {{ pids.push(String(chunk).trim()); printed = true; done(); }});
          child.on("exit", () => {{ exited = true; done(); }});
        }});
      }}
      let refused = "started";
      try {{ spawn("python3", ["-c", "pass"]); }} catch (error) {{ refused = String(error.message || error); }}
      return JSON.stringify({{ refused, pids }});
    "#
        ),
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    drop(plugins);
    let parsed: serde_json::Value = serde_json::from_str(&answer).expect(&answer);
    for pid in parsed["pids"].as_array().into_iter().flatten() {
        if let Some(pid) = pid.as_str().and_then(|pid| pid.parse::<i32>().ok()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let refused = parsed["refused"].as_str().unwrap_or_default();
    assert!(refused.contains("16"), "{answer}");
}

#[tokio::test]
async fn child_exit_125_is_the_programs() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "fails",
        r#"{"run": ["sh"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      return await new Promise((resolve) => {
        execFile("sh", ["-c", "echo out; echo err >&2; exit 125"], (error, stdout, stderr) =>
          resolve(JSON.stringify({ code: error && error.code, message: String(error && error.message), stdout, stderr })));
      });
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    let parsed: serde_json::Value = serde_json::from_str(&answer).expect(&answer);
    assert_eq!(parsed["code"], 125, "{answer}");
    assert!(!answer.contains("sandbox"), "{answer}");
    assert_eq!(parsed["stdout"], "out\n", "{answer}");
    assert_eq!(parsed["stderr"], "err\n", "{answer}");
}

#[tokio::test]
async fn child_stopped_call_kills() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    let pid_file = session.path().join("pid.txt");
    awaited_tool(
        root.path(),
        "spins",
        r#"{"fs": {"write": ["./"]}, "run": ["sleep"]}"#,
        &format!(
            r#"import {{ spawn }} from "node:child_process";
{FS}"#
        ),
        &format!(
            r#"
      const child = spawn("sleep", ["1000"]);
      writeFileSync({file:?}, String(child.pid));
      for (;;) {{}}
    "#,
            file = pid_file.to_string_lossy()
        ),
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let out = run(&plugins, "try").await;
    assert!(out.is_error, "the call was not stopped: {out:?}");
    let pid = pid_of(&pid_file).await;
    gone(pid).await;
}

#[tokio::test]
async fn child_unload_kills() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    let pid_file = session.path().join("pid.txt");
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"fs": {"write": ["./"]}, "run": ["sleep"]}"#,
        &format!(
            r#"import {{ spawn }} from "node:child_process";
{FS}"#
        ),
        &format!(
            r#"
      const child = spawn("sleep", ["1000"]);
      writeFileSync({file:?}, String(child.pid));
      return "started";
    "#,
            file = pid_file.to_string_lossy()
        ),
    )
    .await;
    let plugins = {
        let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
        assert_eq!(&*text(&plugins).await, "started");
        plugins
    };
    let pid = pid_of(&pid_file).await;
    assert!(alive(pid), "{pid} was gone before the unload");
    drop(plugins);
    gone(pid).await;
}

#[tokio::test]
async fn child_max_buffer() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "fills",
        r#"{"run": ["sh"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      return await new Promise((resolve) => {
        execFile("sh", ["-c", "yes | head -c 2000000"], (error) => resolve(String(error && error.code)));
      });
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "ERR_CHILD_PROCESS_STDIO_MAXBUFFER");
}

#[tokio::test]
async fn child_promisify() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "promises",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";
import * as util from "node:util";"#,
        r#"
      const run = typeof util.promisify === "function" ? util.promisify(execFile) : execFile;
      const { stdout, stderr } = await run("/bin/echo", ["x"]);
      return `${stdout}${stderr}`;
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "x\n");
}

#[tokio::test]
async fn child_patched_builtins() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    // A plugin with no `run` permission, whose patches all try to start a
    // program of their own.
    plugin(
        root.path(),
        "patches",
        "{}",
        &[(
            "index.js",
            r#"
let spawn = null;
try { spawn = (await import("node:child_process")).spawn; } catch (error) { spawn = null; }
function tryIt() { if (spawn !== null) { try { spawn("/bin/echo", ["sneak"]); } catch (error) {} } }
Array.prototype.map = function () { tryIt(); return []; };
Array.prototype.join = function () { tryIt(); return ""; };
Map.prototype.get = function () { tryIt(); return undefined; };
Object.defineProperty(Object.prototype, "then", { configurable: true, get() { tryIt(); return undefined; } });
Function.prototype.call = function () { tryIt(); };
export default function (reagent) {
  reagent.registerTool({ name: "patched", description: "patches", parameters: { type: "object" }, run: () => "patched" });
}
"#,
        )],
    )
    .await;
    awaited_tool(
        root.path(),
        "runs",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], (error, stdout) => resolve(`${error} ${stdout}`)));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    assert_eq!(answer, "null hi\n");
    let lines = log_lines(&session.path().join("helper.log"));
    assert_eq!(
        lines.iter().filter(|line| line.as_str() == "sneak").count(),
        0,
        "{lines:?}"
    );
    assert!(lines.contains(&"hi".to_string()), "{lines:?}");
}

#[tokio::test]
async fn child_timeout() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "waits",
        r#"{"run": ["sleep"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      const began = Date.now();
      const answer = await new Promise((resolve) => {
        execFile("sleep", ["30"], { timeout: 200 }, (error) => {
          resolve(`${error && error.killed} ${error && error.signal} ${Date.now() - began}`);
        });
      });
      return answer;
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    let mut parts = answer.split(' ');
    assert_eq!(parts.next(), Some("true"), "{answer}");
    assert_eq!(parts.next(), Some("SIGTERM"), "{answer}");
    let elapsed: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    assert!(elapsed < 5000, "{answer}");
}

/// One variable of the test process that a shell does not fill in itself, or
/// `None` when the process has no environment at all.
fn kept_variable() -> Option<&'static str> {
    ["TMPDIR", "LANG", "TERM", "LOGNAME", "USER"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
}

/// The pair window `[flag, value]` anywhere in `lines`.
fn pair(lines: &[String], flag: &str, value: &str) -> bool {
    lines
        .windows(2)
        .any(|window| window[0] == flag && window[1] == value)
}

#[tokio::test]
async fn child_env_helper_starts_empty() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    let kept = kept_variable().expect("the test process has no environment");
    awaited_tool(
        root.path(),
        "envs",
        r#"{"run": ["/bin/sh"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/sh", ["-c", "printf '%s' \"$PAIR\""], { env: { PAIR: "given" } }, (error, stdout) => resolve(`${error} ${stdout}`)));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "null given");
    // The helper itself starts with nothing reagent holds: neither a variable
    // of the test process nor the pair the plugin sent is there before its
    // `exec`. Only what the shell itself fills in (`PWD`, `SHLVL`, `_`) is.
    let env = tokio::fs::read_to_string(helper_env(session.path()))
        .await
        .expect("the stand-in helper ran");
    assert!(!env.contains(&format!("{kept}=")), "{env}");
    assert!(!env.contains("PAIR="), "{env}");
    assert!(!env.contains("PATH="), "{env}");
}

#[tokio::test]
async fn child_env_pair_reaches_child() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "envs",
        r#"{"run": ["/bin/sh"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/sh", ["-c", "printf '%s' \"$PAIR\""], { env: { PAIR: "given" } }, (error, stdout) => resolve(`${error} ${stdout}`)));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "null given");
    let lines = log_lines(&log);
    assert!(lines.contains(&"--env-clear".to_string()), "{lines:?}");
    assert!(pair(&lines, "--env", "PAIR=given"), "{lines:?}");
}

#[tokio::test]
async fn child_env_loader_names_refused() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { spawn } from "node:child_process";"#,
        r#"
      const refused = [];
      for (const name of ["DYLD_INSERT_LIBRARIES", "LD_PRELOAD", "GCONV_PATH"]) {
        try { spawn("/bin/echo", ["hi"], { env: { [name]: "x" } }); refused.push(`no ${name}`); }
        catch (error) { refused.push(`${error instanceof TypeError} ${String(error.message)} ${name}`); }
      }
      return refused.join("|");
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let answer = text(&plugins).await;
    for name in ["DYLD_INSERT_LIBRARIES", "LD_PRELOAD", "GCONV_PATH"] {
        let field = answer
            .split('|')
            .find(|field| field.ends_with(name))
            .unwrap_or_else(|| panic!("{name}: {answer}"));
        assert!(field.starts_with("true"), "{name}: {answer}");
        assert!(field.contains(name), "{name}: {answer}");
    }
    assert!(log_lines(&log).is_empty(), "{:?}", log_lines(&log));
}

#[tokio::test]
async fn child_options_own_shell() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      Object.prototype.shell = "/bin/sh";
      return await new Promise((resolve) => execFile("/bin/echo", ["hi"], {}, (error, stdout) => resolve(`${error} ${stdout}`)));
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "null hi\n");
    let lines = log_lines(&log);
    let dash = lines
        .iter()
        .position(|line| line == "--")
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert_eq!(lines[dash + 1], canonical("/bin/echo").await, "{lines:?}");
    assert!(!lines.contains(&"sh".to_string()), "{lines:?}");
}

#[tokio::test]
async fn child_options_own_env() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/sh"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      Object.prototype.env = { X: "1" };
      return await new Promise((resolve) => execFile("/bin/sh", ["-c", "printf '%s' \"$X\""], {}, (error, stdout) => resolve(`${error} ${stdout}`)));
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "null ");
}

#[tokio::test]
async fn child_options_own_cwd() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/pwd"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      Object.prototype.cwd = "/";
      return await new Promise((resolve) => execFile("/bin/pwd", [], {}, (error, stdout) => resolve(`${error} ${stdout}`)));
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let inside = session
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(&*text(&plugins).await, format!("null {inside}\n"));
}

#[tokio::test]
async fn child_write_root_link_dropped() {
    let root = temp_root();
    let session = temp_root();
    let other = temp_root();
    let out = session.path().join("out");
    tokio::fs::create_dir(out.clone()).await.unwrap();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "writes",
        r#"{"fs": {"write": ["./out"]}, "run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], () => resolve("done")));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    // The root the load resolved, then a link to another folder in its place.
    let resolved = out.canonicalize().unwrap().to_string_lossy().into_owned();
    tokio::fs::remove_dir(&out).await.unwrap();
    tokio::fs::symlink(other.path(), &out).await.unwrap();
    let linked = other
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(&*text(&plugins).await, "done");
    let lines = log_lines(&log);
    assert!(!pair(&lines, "--write-resolved", &resolved), "{lines:?}");
    assert!(!pair(&lines, "--write-resolved", &linked), "{lines:?}");
}

#[tokio::test]
async fn child_slots_freed() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      let done = 0;
      for (let i = 0; i < 40; i += 1) {
        await new Promise((resolve) => execFile("/bin/echo", ["x"], () => { done += 1; resolve(null); }));
      }
      return String(done);
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let before = descriptors();
    assert_eq!(&*text(&plugins).await, "40");
    let began = Instant::now();
    while began.elapsed() < WAIT && super::child::count() != 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(super::child::count(), 0, "a slot outlived its child");
    let after = descriptors();
    assert!(
        after <= before + 10,
        "the run opened {} more descriptors",
        after.saturating_sub(before)
    );
}

/// The descriptors this process holds, as `/dev/fd` lists them.
#[expect(clippy::disallowed_methods, reason = "test count of open descriptors")]
fn descriptors() -> usize {
    std::fs::read_dir("/dev/fd")
        .map(|entries| entries.count())
        .unwrap_or(0)
}

#[tokio::test]
async fn child_kill_after_exit() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { spawn } from "node:child_process";"#,
        r#"
      const child = spawn("/bin/echo", ["hi"]);
      await new Promise((resolve) => child.on("exit", resolve));
      return String(child.kill());
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "false");
}

#[tokio::test]
async fn child_group_ends_with_its_program() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "leaves",
        r#"{"run": ["sh"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"
      return await new Promise((resolve) => {
        execFile("sh", ["-c", "sleep 1000 >/dev/null 2>&1 & echo $!"], (error, stdout) =>
          resolve(error ? String(error) : stdout.trim()));
      });
    "#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    let pid: u32 = text(&plugins).await.parse().expect("a pid");
    gone(pid).await;
    drop(plugins);
}

#[tokio::test]
async fn child_of_a_failed_twin_ends_at_load() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    let first = session.path().join("first.txt");
    let second = session.path().join("second.txt");
    let permissions = r#"{"fs": {"write": ["./"]}, "run": ["sleep"]}"#;
    // Two plugins of one name: the loader frees the name of a plugin that
    // failed, so the second one loads with the name of the first. The first
    // starts a child and then fails: its runtime ends with its load, and the
    // child with it, so no child of a dead plugin is left for the stop of the
    // second to match.
    let armed = format!(
        r#"import {{ spawn }} from "node:child_process";
{FS}
const child = spawn("sleep", ["1000"]);
writeFileSync({file:?}, String(child.pid));
throw new Error("this plugin fails on purpose");
"#,
        file = first.to_string_lossy()
    );
    let spins = format!(
        r#"import {{ spawn }} from "node:child_process";
{FS}
export default function (reagent) {{
  reagent.registerTool({{
    name: "second",
    description: "Runs the test's code.",
    parameters: {{ type: "object" }},
    async run() {{
      const child = spawn("sleep", ["1000"]);
      writeFileSync({file:?}, String(child.pid));
      // Long enough for the timer of the other plugin of this name to fire,
      // then a call the watchdog stops.
      await new Promise((resolve) => setTimeout(resolve, 500));
      for (;;) {{}}
    }},
  }});
}}
"#,
        file = second.to_string_lossy()
    );
    plugin_named(
        root.path(),
        "one",
        "twins",
        permissions,
        &[("index.js", &armed)],
    )
    .await;
    plugin_named(
        root.path(),
        "two",
        "twins",
        permissions,
        &[("index.js", &spins)],
    )
    .await;
    let (plugins, notes) = loaded(root.path(), session.path(), Some(path)).await;
    assert!(
        notes.iter().any(|note| note.contains("fails on purpose")),
        "{notes:?}"
    );
    gone(pid_of(&first).await).await;
    let out = run(&plugins, "second").await;
    assert!(out.is_error, "the call was not stopped: {out:?}");
    gone(pid_of(&second).await).await;
}

#[tokio::test]
async fn child_exec_with_bin_sh() {
    let root = temp_root();
    let session = temp_root();
    let (path, _) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "runs",
        r#"{"run": ["/bin/sh"]}"#,
        r#"import { exec } from "node:child_process";"#,
        r#"return await new Promise((resolve) => exec("echo hi", (error, stdout) => resolve(`${error} ${stdout}`)));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "null hi\n");
}

#[tokio::test]
async fn child_keep_group_flag() {
    let root = temp_root();
    let session = temp_root();
    let (path, log) = helper(session.path()).await;
    awaited_tool(
        root.path(),
        "spawns",
        r#"{"run": ["/bin/echo"]}"#,
        r#"import { execFile } from "node:child_process";"#,
        r#"return await new Promise((resolve) => execFile("/bin/echo", ["hi"], () => resolve("done")));"#,
    )
    .await;
    let (plugins, _) = loaded(root.path(), session.path(), Some(path)).await;
    assert_eq!(&*text(&plugins).await, "done");
    let lines = log_lines(&log);
    assert!(lines.contains(&"--keep-group".to_string()), "{lines:?}");
}
