//! The tests of the Node shims: the small parts of Node reagent writes itself,
//! since LLRT has none of them.
//!
//! Built with the `node` feature only. The helpers are the ones the other Node
//! tests use, copied here so neither file grows a second owner.

use std::path::Path;
use std::sync::Arc;

use reagent_agent::tool::{Tool, ToolOutput};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::super::Plugins;

/// Writes one plugin folder: its `package.json` and one file per pair.
async fn plugin(root: &Path, folder: &str, permissions: &str, files: &[(&str, &str)]) {
    let dir = root.join(folder);
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, body).await.unwrap();
    }
    let package = format!(
        r#"{{"name": "{folder}", "version": "1.0.0", "type": "module", "reagent": {{"entry": "index.js", "permissions": {permissions}}}}}"#
    );
    tokio::fs::write(dir.join("package.json"), package)
        .await
        .unwrap();
}

/// The entry of a plugin with one tool named `try`: the `imports` of the file
/// and the synchronous `body` of the tool.
fn entry(imports: &str, body: &str) -> String {
    format!(
        r#"{imports}
export default function (reagent) {{
  reagent.registerTool({{
    name: "try",
    description: "Runs the test's code.",
    parameters: {{ type: "object" }},
    run() {{ {body} }},
  }});
}}
"#
    )
}

/// Writes one plugin with one tool that runs the synchronous `body`.
async fn tool(root: &Path, folder: &str, permissions: &str, imports: &str, body: &str) {
    plugin(
        root,
        folder,
        permissions,
        &[("index.js", &entry(imports, body))],
    )
    .await;
}

/// Writes one plugin with one awaited tool named `name`.
async fn awaited_tool(
    root: &Path,
    folder: &str,
    permissions: &str,
    name: &str,
    imports: &str,
    body: &str,
) {
    let source = format!(
        r#"{imports}
export default function (reagent) {{
  reagent.registerTool({{
    name: {name:?},
    description: "Runs the test's code.",
    parameters: {{ type: "object" }},
    async run() {{ {body} }},
  }});
}}
"#
    );
    plugin(root, folder, permissions, &[("index.js", &source)]).await;
}

/// A temporary folder for the plugins.
fn temp_root() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// Loads the plugins under `root`, with `session` the folder a relative path
/// resolves against, which must have loaded some.
async fn loaded(root: &Path, session: &Path) -> Arc<Plugins> {
    let state = tempfile::tempdir().unwrap().keep();
    let (plugins, notes) = Plugins::load_in(root, session, &state).await;
    plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"))
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

/// The text one tool named `try` answered with.
async fn text(plugins: &Plugins) -> String {
    run(plugins, "try").await.content.to_string()
}

/// The `SIGUSR1` number of the platform this test runs on.
fn sigusr1() -> u32 {
    if cfg!(target_os = "macos") { 30 } else { 10 }
}

/// The `SIGSTOP` number of the platform this test runs on.
fn sigstop() -> u32 {
    if cfg!(target_os = "macos") { 17 } else { 19 }
}

#[tokio::test]
async fn shim_path_has_posix() {
    let root = temp_root();
    tool(
        root.path(),
        "paths",
        "{}",
        r#"import path from "node:path";
import * as bare from "path";"#,
        r#"const out = [
    path.posix.join("a", "b"),
    path.posix === path.posix.posix ? "self" : "other",
    bare.posix === path.posix ? "one object" : "two objects",
    bare.sep,
    bare.delimiter,
    path.posix.sep,
    path.posix.delimiter,
    path.posix.basename("/a/b.txt"),
    bare.default.posix === path.posix ? "default" : "no default",
  ];
  return out.join(" ");"#,
    )
    .await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    assert_eq!(
        &*text(&plugins).await,
        "a/b self one object / : / : b.txt default"
    );
}

#[tokio::test]
async fn shim_os_has_constants() {
    let root = temp_root();
    tool(
        root.path(),
        "osc",
        "{}",
        r#"import os from "node:os";
import * as bare from "os";"#,
        r#"const out = [
    os.constants.signals.SIGTERM,
    os.constants.signals.SIGKILL,
    os.constants.signals.SIGUSR1,
    os.constants.signals.SIGSTOP,
    os.constants.errno.ENOENT,
    os.constants.errno.EACCES,
    os.constants.errno.ELOOP,
    os.constants.priority.PRIORITY_NORMAL,
    bare.constants === os.constants ? "same" : "differ",
    Object.isFrozen(os.constants) ? "frozen" : "open",
    os.platform(),
  ];
  return out.join(" ");"#,
    )
    .await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    let expected = format!(
        "15 9 {} {} 2 13 {} 0 same frozen {}",
        sigusr1(),
        sigstop(),
        if cfg!(target_os = "macos") { 62 } else { 40 },
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    );
    assert_eq!(&*text(&plugins).await, expected);
}

#[tokio::test]
async fn shim_constants_merges_os_and_fs() {
    let root = temp_root();
    tool(
        root.path(),
        "consts",
        "{}",
        r#"import { O_CREAT, errno, signals } from "node:constants";
import * as bare from "constants";
import * as fs from "node:fs";"#,
        r#"const out = [
    typeof O_CREAT,
    bare.O_CREAT === O_CREAT ? "same" : "differ",
    signals.SIGTERM,
    errno.ENOENT,
    bare.default.signals === signals ? "signals" : "other signals",
    bare.default.errno === errno ? "errno" : "other errno",
    fs.constants.F_OK,
    typeof bare.default.O_RDONLY,
    typeof bare.default.COPYFILE_EXCL,
  ];
  return out.join(" ");"#,
    )
    .await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    assert_eq!(
        &*text(&plugins).await,
        "number same 15 2 signals errno 0 number number"
    );
}

#[tokio::test]
async fn shim_timers_promises_wait_and_abort() {
    let root = temp_root();
    awaited_tool(
        root.path(),
        "waiting",
        "{}",
        "waiting",
        r#"import { scheduler, setImmediate, setInterval, setTimeout } from "node:timers/promises";
import * as bare from "timers/promises";"#,
        r#"const out = [];
  out.push(await setTimeout(10, "x"));
  out.push(await setImmediate("i"));
  out.push(await bare.setTimeout(1, "b"));
  const controller = new AbortController();
  const pending = setTimeout(1000, "late", { signal: controller.signal });
  controller.abort();
  try { await pending; out.push("resolved"); } catch (error) { out.push(error.name); }
  const done = new AbortController();
  done.abort();
  try { await setTimeout(1000, "never", { signal: done.signal }); out.push("resolved"); }
  catch (error) { out.push(error.name); }
  let count = 0;
  for await (const value of setInterval(5, "tick")) {
    out.push(value);
    count += 1;
    if (count === 3) break;
  }
  out.push(`ticks ${count}`);
  out.push((await bare.scheduler.wait(5)) === undefined ? "waited" : "value");
  await scheduler.yield();
  out.push("yielded");
  return out.join(" ");"#,
    )
    .await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    let out = run(&plugins, "waiting").await;
    assert!(!out.is_error, "{:?}", out.content);
    assert_eq!(
        &*out.content,
        "x i b AbortError AbortError tick tick tick ticks 3 waited yielded"
    );
}

#[tokio::test]
async fn shim_util_types_checks() {
    let root = temp_root();
    tool(
        root.path(),
        "types",
        "{}",
        r#"import { types } from "node:util/types";
import * as bare from "util/types";
import util from "node:util";"#,
        r#"const generator = function* () {};
  const out = [
    types.isPromise(Promise.resolve()) ? "promise" : "no promise",
    types.isPromise(1) ? "promise" : "no promise",
    types.isDate(new Date()) ? "date" : "no date",
    types.isRegExp(/x/) ? "regexp" : "no regexp",
    types.isMap(new Map()) ? "map" : "no map",
    types.isSet(new Set()) ? "set" : "no set",
    types.isWeakMap(new WeakMap()) ? "weakmap" : "no weakmap",
    types.isWeakSet(new WeakSet()) ? "weakset" : "no weakset",
    types.isArrayBuffer(new ArrayBuffer(1)) ? "buffer" : "no buffer",
    types.isAnyArrayBuffer(new ArrayBuffer(1)) ? "any" : "no any",
    types.isTypedArray(new Uint8Array(1)) ? "typed" : "no typed",
    types.isUint8Array(new Uint8Array(1)) ? "uint8" : "no uint8",
    types.isNativeError(new TypeError("x")) ? "error" : "no error",
    types.isAsyncFunction(async () => {}) ? "async" : "no async",
    types.isGeneratorFunction(generator) ? "generator" : "no generator",
    types.isBoxedPrimitive(Object(1)) ? "boxed" : "no boxed",
    types.isProxy(new Proxy({}, {})) ? "proxy" : "no proxy",
    bare.types === types ? "same" : "differ",
    util.types === types ? "util" : "differ",
  ];
  return out.join(" ");"#,
    )
    .await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    assert_eq!(
        &*text(&plugins).await,
        "promise no promise date regexp map set weakmap weakset buffer any typed uint8 \
         error async generator boxed no proxy same util"
    );
}

#[tokio::test]
async fn shim_diagnostics_channel_publishes() {
    let root = temp_root();
    tool(
        root.path(),
        "channels",
        "{}",
        r#"import { channel, hasSubscribers, subscribe, tracingChannel } from "node:diagnostics_channel";
import * as bare from "diagnostics_channel";"#,
        r#"const out = [];
  const first = channel("x");
  out.push(first === channel("x") ? "same" : "differ");
  out.push(hasSubscribers("x") ? "has" : "none");
  let seen = 0;
  const onMessage = (message) => { seen += message; };
  subscribe("x", onMessage);
  out.push(hasSubscribers("x") ? "has" : "none");
  first.publish(2);
  bare.channel("x").publish(1);
  out.push(String(seen));
  first.unsubscribe(onMessage);
  out.push(hasSubscribers("x") ? "has" : "none");
  let called = 0;
  const tracing = tracingChannel("y");
  const value = tracing.traceSync((a, b) => { called = a + b; return "synced"; }, {}, undefined, 1, 2);
  out.push(value);
  out.push(String(called));
  return out.join(" ");"#,
    )
    .await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    assert_eq!(&*text(&plugins).await, "same none has 3 none synced 3");
}

/// Writes one plugin that subscribes to the channel `x` at load, and counts what
/// it hears in `NAME_count`, publishing with `NAME_publish`.
async fn channel_plugin(root: &Path, folder: &str, name: &str) {
    let source = r#"import { channel } from "node:diagnostics_channel";

const channelX = channel("x");
let seen = 0;
channelX.subscribe(() => { seen += 1; });

export default function (reagent) {
  reagent.registerTool({
    name: "NAME_count",
    description: "Counts.",
    parameters: { type: "object" },
    run() { return String(seen); },
  });
  reagent.registerTool({
    name: "NAME_publish",
    description: "Publishes.",
    parameters: { type: "object" },
    run() { channelX.publish(1); return "published"; },
  });
}
"#
    .replace("NAME", name);
    plugin(root, folder, "{}", &[("index.js", &source)]).await;
}

#[tokio::test]
async fn shim_plugins_have_own_channels() {
    let root = temp_root();
    channel_plugin(root.path(), "first", "first").await;
    channel_plugin(root.path(), "second", "second").await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    let out = run(&plugins, "first_publish").await;
    assert_eq!(&*out.content, "published");
    assert_eq!(&*run(&plugins, "first_count").await.content, "1");
    assert_eq!(&*run(&plugins, "second_count").await.content, "0");
    let out = run(&plugins, "second_publish").await;
    assert_eq!(&*out.content, "published");
    assert_eq!(&*run(&plugins, "second_count").await.content, "1");
    assert_eq!(&*run(&plugins, "first_count").await.content, "1");
}

#[tokio::test]
async fn shim_npm_style_imports() {
    let root = temp_root();
    awaited_tool(
        root.path(),
        "npm",
        "{}",
        "npm",
        r#"import constants from "node:constants";
import { channel } from "node:diagnostics_channel";
import { setTimeout as later } from "node:timers/promises";
import types from "node:util/types";
import path, { delimiter, posix, sep } from "path";
import os from "os";"#,
        r#"const seen = [];
  channel("npm").subscribe((message) => seen.push(message));
  channel("npm").publish("hit");
  const out = [
    String(await later(1, "ok")),
    typeof constants.O_CREAT,
    os.constants.signals.SIGPIPE,
    types.isPromise(Promise.resolve()) ? "promise" : "no promise",
    posix.join("a", "b"),
    sep + delimiter,
    path.posix.join("a", "b"),
    seen.join(","),
  ];
  return out.join(" ");"#,
    )
    .await;
    let session = temp_root();
    let plugins = loaded(root.path(), session.path()).await;
    let out = run(&plugins, "npm").await;
    assert!(!out.is_error, "{:?}", out.content);
    assert_eq!(&*out.content, "ok number 13 promise a/b /: a/b hit");
}
