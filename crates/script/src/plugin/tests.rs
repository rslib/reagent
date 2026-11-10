//! The plugin tests: every one writes its plugins into a temporary folder and
//! drives the runtime directly.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reagent_agent::hook::{CallHook, CallVerdict, TurnHook};
use reagent_agent::policy::TurnView;
use reagent_agent::tool::{Object, Tool, ToolOutput};
use reagent_ai::{AssistantMessage, Content};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{CommandInfo, PanelView, Plugins};

/// A term for the turn view the turn hook reads.
const NO_TOOLS: &[(reagent_ai::ToolCall, reagent_ai::ToolResult)] = &[];

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

/// A temporary plugins root.
fn temp_root() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// Loads the plugins under `root`, which must have loaded some.
async fn loaded(root: &Path) -> std::sync::Arc<Plugins> {
    let (plugins, notes) = Plugins::load(root).await;
    plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"))
}

/// Loads the plugins under `root`, which must have loaded some, with the notes
/// the load raised.
async fn loaded_with_notes(root: &Path) -> (std::sync::Arc<Plugins>, Vec<String>) {
    let (plugins, notes) = Plugins::load(root).await;
    let plugins = plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"));
    (plugins, notes)
}

/// Runs one plugin tool and answers with its result.
async fn run(plugins: &Plugins, name: &str, args: &str) -> ToolOutput {
    let tool = plugins
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == name)
        .unwrap_or_else(|| panic!("no plugin tool named {name}"));
    tool.run(args, &CancellationToken::new()).await
}

/// The names of the plugin tools, in load order.
fn names(plugins: &Plugins) -> Vec<String> {
    plugins
        .tools()
        .iter()
        .map(|tool| tool.spec().name.clone())
        .collect()
}

/// `path` spelled from the folder the test runs in, so a root may be relative.
fn from_here(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap();
    let mut up = PathBuf::new();
    for _ in cwd.components().skip(1) {
        up.push("..");
    }
    up.join(path.strip_prefix("/").unwrap_or(path))
}

/// An image, for a result that must keep what a layer did not touch.
fn picture() -> reagent_ai::Image {
    reagent_ai::Image {
        media_type: reagent_ai::ImageType::Png,
        width: 1,
        height: 1,
        data: "iVBORw0KGgo=".into(),
    }
}

/// A result with text, an object and an image: everything but the text is
/// what a layer must leave alone.
fn layered() -> ToolOutput {
    let fields = serde_json::Map::from_iter([("answer".to_string(), json!(42))]);
    let mut output = ToolOutput::ok("hi").with_images(vec![picture()]);
    output.object = Some(Box::new(Object::new(fields)));
    output
}

#[tokio::test]
async fn a_plugin_tool_answers() {
    let root = temp_root();
    plugin(
        root.path(),
        "hello",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (reagent) {
  reagent.registerTool({
    name: "shout",
    description: "Shouts its text back.",
    parameters: { type: "object", properties: { text: { type: "string" } } },
    run(args) { return String(args.text).toUpperCase(); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(names(&plugins), ["shout"]);
    let tool = plugins.tools().remove(0);
    assert_eq!(tool.spec().description, "Shouts its text back.");
    assert_eq!(tool.spec().input_schema["type"], json!("object"));
    let out = run(&plugins, "shout", r#"{"text":"hi"}"#).await;
    assert_eq!(&*out.content, "HI");
    assert!(!out.is_error);
    assert!(plugins.take_notes().is_empty());
}

#[tokio::test]
async fn an_async_tool_answers() {
    let root = temp_root();
    plugin(
        root.path(),
        "late",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (reagent) {
  reagent.registerTool({
    name: "later",
    description: "Answers after a microtask.",
    parameters: { type: "object" },
    async run(args) {
      const text = await Promise.resolve(String(args.text).toUpperCase());
      return { text };
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let out = run(&plugins, "later", r#"{"text":"hi"}"#).await;
    assert_eq!(&*out.content, "HI");
    assert!(!out.is_error);
}

#[tokio::test]
async fn a_tool_that_throws_is_an_error_result() {
    let root = temp_root();
    plugin(
        root.path(),
        "angry",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (reagent) {
  reagent.registerTool({
    name: "angry",
    description: "Throws.",
    parameters: { type: "object" },
    run() { throw new Error("no way"); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let out = run(&plugins, "angry", "{}").await;
    assert!(out.is_error);
    assert_eq!(&*out.content, "no way");
}

#[tokio::test]
async fn a_folder_without_a_manifest_is_skipped() {
    let root = temp_root();
    tokio::fs::create_dir(root.path().join("plain"))
        .await
        .unwrap();
    tokio::fs::write(
        root.path().join("plain/index.js"),
        "export default () => {};",
    )
    .await
    .unwrap();
    let (none, notes) = Plugins::load(root.path()).await;
    assert!(none.is_none(), "a bare folder is no plugin");
    assert!(notes.is_empty(), "a bare folder is skipped in silence");

    plugin(
        root.path(),
        "hello",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "hi", description: "says hi", parameters: { type: "object" }, run: () => "hi" }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(names(&plugins), ["hi"]);
    assert!(plugins.take_notes().is_empty());
}

#[tokio::test]
async fn a_bad_manifest_gives_a_note() {
    let root = temp_root();
    plugin(root.path(), "broken", "{not json", &[("index.js", "")]).await;
    let (none, notes) = Plugins::load(root.path()).await;
    assert!(none.is_none());
    assert!(
        notes.iter().any(|note| note.starts_with("plugin broken: ")),
        "{notes:?}"
    );

    let second = temp_root();
    plugin(
        second.path(),
        "gone",
        r#"{"name": "gone", "reagent": {"entry": "missing.js"}}"#,
        &[("index.js", "")],
    )
    .await;
    let (none, notes) = Plugins::load(second.path()).await;
    assert!(none.is_none());
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin gone: ") && note.contains("missing.js")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn an_entry_that_throws_skips_only_that_plugin() {
    let root = temp_root();
    plugin(
        root.path(),
        "a_good",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "kept", description: "stays", parameters: { type: "object" }, run: () => "kept" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "b_bad",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({ name: "gone", description: "goes", parameters: { type: "object" }, run: () => "gone" });
  throw new Error("load trouble");
}
"#,
        )],
    )
    .await;
    // The notes a load raised come back from the load, not from `take_notes`.
    let (plugins, notes) = loaded_with_notes(root.path()).await;
    assert_eq!(names(&plugins), ["kept"]);
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin b_bad: ") && note.contains("load trouble")),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|note| note.contains("a_good")),
        "{notes:?}"
    );
    assert!(
        plugins.take_notes().is_empty(),
        "the load drained its notes"
    );
}

#[tokio::test]
async fn a_relative_import_loads_and_an_escape_fails() {
    let root = temp_root();
    plugin(
        root.path(),
        "importer",
        manifest(),
        &[
            (
                "index.js",
                r#"
import { shout } from "./lib/util.js";
export default function (r) {
  r.registerTool({ name: "shouty", description: "shouts", parameters: { type: "object" }, run: (args) => shout(String(args.text)) });
}
"#,
            ),
            (
                "lib/util.js",
                "export function shout(text) { return text.toUpperCase(); }",
            ),
        ],
    )
    .await;
    plugin(
        root.path(),
        "escape",
        manifest(),
        &[(
            "index.js",
            r#"import x from "../x.js"; export default () => {};"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "bare",
        manifest(),
        &[
            (
                "index.js",
                r#"import yaml from "yaml"; export default () => {};"#,
            ),
            // A real package, so only the resolver can refuse the specifier.
            ("node_modules/yaml/index.js", "export default 1;"),
        ],
    )
    .await;
    let (plugins, notes) = loaded_with_notes(root.path()).await;
    assert_eq!(names(&plugins), ["shouty"]);
    let out = run(&plugins, "shouty", r#"{"text":"hi"}"#).await;
    assert_eq!(&*out.content, "HI");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin escape: ") && note.contains("../x.js")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin bare: ") && note.contains("yaml")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_relative_root_loads_its_plugins() {
    let root = temp_root();
    plugin(
        root.path(),
        "here",
        manifest(),
        &[
            (
                "index.js",
                r#"
import { shout } from "./lib/util.js";
export default function (r) {
  r.registerTool({ name: "shouts", description: "shouts", parameters: { type: "object" }, run: (args) => shout(String(args.text)) });
}
"#,
            ),
            (
                "lib/util.js",
                "export function shout(text) { return text.toUpperCase(); }",
            ),
        ],
    )
    .await;
    let relative = from_here(root.path());
    assert!(relative.is_relative(), "{relative:?}");
    let (plugins, notes) = loaded_with_notes(&relative).await;
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        &*run(&plugins, "shouts", r#"{"text":"hi"}"#).await.content,
        "HI"
    );
}

#[tokio::test]
async fn a_symlink_out_of_the_plugin_folder_fails() {
    let root = temp_root();
    let outside = temp_root();
    tokio::fs::write(
        outside.path().join("own.js"),
        "export const name = 'outside';",
    )
    .await
    .unwrap();
    plugin(
        root.path(),
        "linked",
        manifest(),
        &[(
            "index.js",
            r#"import { name } from "./lib/own.js"; export default function (r) { r.registerTool({ name: "leaks", description: "leaks", parameters: { type: "object" }, run: () => name }); }"#,
        )],
    )
    .await;
    tokio::fs::symlink(outside.path(), root.path().join("linked/lib"))
        .await
        .unwrap();
    let (plugins, notes) = Plugins::load(root.path()).await;
    assert!(
        plugins.is_none(),
        "a link out of the folder is not a plugin: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin linked: ") && note.contains("./lib/own.js")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn an_import_that_walks_out_of_the_folder_fails() {
    let root = temp_root();
    tokio::fs::write(
        root.path().join("outside.js"),
        "export const name = 'outside';",
    )
    .await
    .unwrap();
    plugin(
        root.path(),
        "walk",
        manifest(),
        &[
            (
                "index.js",
                r#"import { name } from "./lib/../../outside.js"; export default function (r) { r.registerTool({ name: "leaks", description: "leaks", parameters: { type: "object" }, run: () => name }); }"#,
            ),
            ("lib/keep.js", "export const keep = 1;"),
        ],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path()).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes.iter().any(
            |note| note.starts_with("plugin walk: ") && note.contains("./lib/../../outside.js")
        ),
        "{notes:?}"
    );
}

#[tokio::test]
async fn an_absolute_import_fails() {
    let root = temp_root();
    let shared = root.path().join("shared.js");
    tokio::fs::write(&shared, "export const name = 'shared';")
        .await
        .unwrap();
    let source = format!(
        r#"import {{ name }} from "{}"; export default function (r) {{ r.registerTool({{ name: "leaks", description: "leaks", parameters: {{ type: "object" }}, run: () => name }}); }}"#,
        shared.display()
    );
    plugin(
        root.path(),
        "abs",
        manifest(),
        &[("index.js", source.as_str())],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path()).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin abs: ")
                && note.contains(shared.to_str().unwrap())),
        "{notes:?}"
    );
}

#[tokio::test]
async fn an_import_resolves_inside_the_plugin_that_imports_it() {
    let root = temp_root();
    plugin(
        root.path(),
        "a",
        manifest(),
        &[
            (
                "index.js",
                r#"
export default function (r) {
  r.registerTool({
    name: "loads",
    description: "imports while it runs",
    parameters: { type: "object" },
    async run() {
      const own = await import("./lib/own.js");
      let escaped = "loaded";
      try {
        await import("../b/x.js");
      } catch (error) {
        escaped = String(error && error.message ? error.message : error);
      }
      return `${own.name} ${escaped}`;
    },
  });
}
"#,
            ),
            ("lib/own.js", r#"export const name = "own";"#),
        ],
    )
    .await;
    plugin(
        root.path(),
        "b",
        manifest(),
        &[
            (
                "index.js",
                r#"export default function (r) { r.registerTool({ name: "b_tool", description: "b", parameters: { type: "object" }, run: () => "b" }); }"#,
            ),
            ("x.js", "export default 1;"),
        ],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(names(&plugins), ["loads", "b_tool"], "b loaded last");
    let out = run(&plugins, "loads", "{}").await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.starts_with("own "),
        "the importing plugin's own module loaded: {}",
        out.content
    );
    assert!(
        out.content.contains("cannot import '../b/x.js'"),
        "the import into the other plugin failed: {}",
        out.content
    );
}

#[tokio::test]
async fn a_call_hook_blocks_and_rewrites() {
    let root = temp_root();
    plugin(
        root.path(),
        "guard",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", ({ tool, args }) => {
    if (tool !== "shout") return undefined;
    if (args.text === "no") return { block: "not allowed" };
    if (args.text === "hi") return { args: { text: "bye" } };
    return undefined;
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let CallVerdict::Args(args) = plugins.before("shout", r#"{"text":"hi"}"#).await else {
        panic!("the layer replaced the arguments");
    };
    let args: serde_json::Value = serde_json::from_str(&args).unwrap();
    assert_eq!(args["text"], json!("bye"));
    assert_eq!(
        plugins.before("shout", r#"{"text":"no"}"#).await,
        CallVerdict::Block("not allowed".into())
    );
    assert_eq!(
        plugins.before("other", r#"{"text":"no"}"#).await,
        CallVerdict::Keep
    );
}

#[tokio::test]
async fn a_block_of_false_keeps_the_call() {
    let root = temp_root();
    plugin(
        root.path(),
        "guard",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", ({ args }) => {
    if (args.kind === "reason") return { block: "not allowed" };
    if (args.kind === "true") return { block: true };
    return { block: false };
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.before("shout", r#"{"kind":"reason"}"#).await,
        CallVerdict::Block("not allowed".into())
    );
    assert_eq!(
        plugins.before("shout", r#"{"kind":"true"}"#).await,
        CallVerdict::Block("blocked by plugin guard".into())
    );
    assert_eq!(
        plugins.before("shout", r#"{"kind":"false"}"#).await,
        CallVerdict::Keep
    );
}

#[tokio::test]
async fn a_result_hook_rewrites_text() {
    let root = temp_root();
    plugin(
        root.path(),
        "decorate",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.result", ({ tool, text }) => {
    if (tool === "fails") return { isError: true };
    return { text: `<${text}>` };
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let out = plugins.after("shout", "{}", ToolOutput::ok("hi")).await;
    assert_eq!(&*out.content, "<hi>");
    assert!(!out.is_error);
    // A layer that leaves `text` out keeps the text the call produced.
    let out = plugins.after("fails", "{}", ToolOutput::ok("hi")).await;
    assert_eq!(&*out.content, "hi");
    assert!(out.is_error);
}

#[tokio::test]
async fn a_result_layer_keeps_the_parts_it_did_not_change() {
    let root = temp_root();
    plugin(
        root.path(),
        "parts",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.result", ({ tool, text }) => {
    if (tool !== "shout") return undefined;
    return { text: `<${text}>` };
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    for (tool, text) in [("plain", "hi"), ("shout", "<hi>")] {
        let out = plugins.after(tool, "{}", layered()).await;
        assert_eq!(&*out.content, text, "the {tool} layer's text");
        assert!(!out.is_error);
        assert_eq!(out.images().len(), 1, "the {tool} layer kept the image");
        assert_eq!(out.images()[0], picture());
        assert!(out.object.is_some(), "the {tool} layer kept the object");
    }
}

#[tokio::test]
async fn a_result_getter_that_loops_is_stopped() {
    let root = temp_root();
    plugin(
        root.path(),
        "lazy",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.result", () => ({ get text() { while (true) {} } }));
  r.registerTool({ name: "ping", description: "pings", parameters: { type: "object" }, run: () => "pong" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let out = plugins.after("ping", "{}", ToolOutput::ok("hi")).await;
    let took = started.elapsed();
    assert_eq!(&*out.content, "hi", "the loopy getter changed nothing");
    assert!(took < Duration::from_secs(7), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin lazy: hook tool.result stopped after 5 s"),
        "{notes:?}"
    );
    assert_eq!(
        &*run(&plugins, "ping", "{}").await.content,
        "pong",
        "the thread is alive"
    );
}

#[tokio::test]
async fn a_turn_hook_follows_up_at_most_three_times() {
    let root = temp_root();
    plugin(
        root.path(),
        "loop",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  let turns = 0;
  r.hook("turn.end", ({ text }) => {
    if (text === "stop") return undefined;
    turns += 1;
    return `again ${turns}`;
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let assistant = AssistantMessage::new("fake", "fake", vec![Content::text("done")]);
    let turn = TurnView {
        index: 1,
        elapsed: Duration::ZERO,
        ran_tools: true,
        assistant: &assistant,
        tools: NO_TOOLS,
        changed_files: None,
    };
    let hook: &dyn TurnHook = &*plugins;
    for at in 1..=3 {
        let follow = hook.turn_end(&turn).await.expect("a follow-up");
        assert_eq!(&*follow, format!("again {at}"));
    }
    assert!(hook.turn_end(&turn).await.is_none(), "the fourth ends it");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugins: 3 follow-ups in a row, the turn ends"),
        "{notes:?}"
    );

    // A turn end with no follow-up clears the count, so three more follow.
    let quiet = AssistantMessage::new("fake", "fake", vec![Content::text("stop")]);
    let reset = TurnView {
        index: 1,
        elapsed: Duration::ZERO,
        ran_tools: true,
        assistant: &quiet,
        tools: NO_TOOLS,
        changed_files: None,
    };
    assert!(hook.turn_end(&reset).await.is_none(), "no follow-up");
    // The layer counted the capped follow-up too, so the next one is the fifth.
    for at in 5..=7 {
        let follow = hook.turn_end(&turn).await.expect("a follow-up");
        assert_eq!(&*follow, format!("again {at}"));
    }
    assert!(hook.turn_end(&turn).await.is_none(), "the fourth ends it");
}

#[tokio::test]
async fn a_hook_that_hangs_is_stopped() {
    let root = temp_root();
    plugin(
        root.path(),
        "hang",
        manifest(),
        &[(
            "index.js",
            r#"
let calls = 0;
export default function (r) {
  r.hook("tool.call", () => { calls += 1; if (calls === 1) { while (true) {} } return undefined; });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let verdict = plugins.before("shout", "{}").await;
    let took = started.elapsed();
    assert_eq!(verdict, CallVerdict::Keep);
    assert!(took < Duration::from_secs(6), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin hang: hook tool.call stopped after 5 s"),
        "{notes:?}"
    );

    // The thread survived the stop: the next call answers at once.
    let started = Instant::now();
    assert_eq!(plugins.before("shout", "{}").await, CallVerdict::Keep);
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "the second call took {took:?}"
    );
    assert!(
        plugins.take_notes().is_empty(),
        "a plain call notes nothing"
    );
}

#[tokio::test]
async fn a_tool_run_that_hangs_is_stopped() {
    let root = temp_root();
    plugin(
        root.path(),
        "hang",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "stuck", description: "stuck", parameters: { type: "object" }, run: () => { while (true) {} } }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let out = run(&plugins, "stuck", "{}").await;
    let took = started.elapsed();
    assert!(out.is_error, "a stopped tool is an error result");
    assert_eq!(&*out.content, "the plugin call was stopped after 5 s");
    assert!(took < Duration::from_secs(6), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin hang: tool stuck stopped after 5 s"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_listener_that_hangs_is_stopped() {
    let root = temp_root();
    plugin(
        root.path(),
        "hang",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.on("turn_end", () => { while (true) {} });
  r.registerTool({ name: "ping", description: "pings", parameters: { type: "object" }, run: () => "pong" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.event("turn_end", json!({ "index": 1 }));
    // The tool call behind the event waits for the stopped listener.
    let started = Instant::now();
    assert_eq!(&*run(&plugins, "ping", "{}").await.content, "pong");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(6), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin hang: event turn_end stopped after 5 s"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_plugin_that_hangs_loading_skips_only_itself() {
    let root = temp_root();
    plugin(
        root.path(),
        "a_good",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "kept", description: "kept", parameters: { type: "object" }, run: () => "kept" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "b_hang",
        manifest(),
        &[("index.js", "while (true) {}\nexport default () => {};")],
    )
    .await;
    let started = Instant::now();
    let (plugins, notes) = Plugins::load(root.path()).await;
    let took = started.elapsed();
    let plugins = plugins.unwrap_or_else(|| panic!("no plugin loaded: {notes:?}"));
    assert_eq!(names(&plugins), ["kept"]);
    assert!(took < Duration::from_secs(7), "it took {took:?}");
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin b_hang: load stopped after 5 s"),
        "{notes:?}"
    );
    assert_eq!(&*run(&plugins, "kept", "{}").await.content, "kept");
}

#[tokio::test]
async fn a_slow_layer_is_noted_and_kept() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => { const t = Date.now(); while (Date.now() - t < 1500) {} });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    assert_eq!(plugins.before("shout", "{}").await, CallVerdict::Keep);
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(4),
        "the call was stopped: {took:?}"
    );
    assert!(
        took >= Duration::from_millis(1400),
        "the call was short: {took:?}"
    );
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin slow: hook tool.call is slow"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn listeners_get_events() {
    let root = temp_root();
    plugin(
        root.path(),
        "counting",
        manifest(),
        &[(
            "index.js",
            r#"
let turns = 0;
export default function (r) {
  r.on("turn_end", () => { turns += 1; });
  r.registerTool({ name: "count", description: "counts", parameters: { type: "object" }, run: () => String(turns) });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.event("turn_end", json!({ "index": 1 }));
    plugins.event("turn_end", json!({ "index": 2 }));
    plugins.event("nothing", json!({}));
    assert_eq!(&*run(&plugins, "count", "{}").await.content, "2");
}

#[tokio::test]
async fn the_notes_stop_at_a_hundred() {
    let root = temp_root();
    plugin(
        root.path(),
        "noisy",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.on("turn_end", () => { throw new Error("boom"); });
  r.registerTool({ name: "sync", description: "syncs", parameters: { type: "object" }, run: () => "ok" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    for index in 0..150 {
        plugins.event("turn_end", json!({ "index": index }));
    }
    // The job behind the events queues after them, so its answer means they ran.
    assert_eq!(&*run(&plugins, "sync", "{}").await.content, "ok");
    let notes = plugins.take_notes();
    assert_eq!(notes.len(), 101, "{notes:?}");
    assert!(
        notes
            .iter()
            .take(100)
            .all(|note| note == "plugin noisy: event turn_end threw: boom"),
        "{:?}",
        &notes[..2]
    );
    assert_eq!(notes[100], "plugins: 50 more notes dropped");
    assert!(plugins.take_notes().is_empty(), "the count is taken once");
}

#[tokio::test]
async fn a_bad_or_taken_tool_name_is_refused() {
    let root = temp_root();
    plugin(
        root.path(),
        "first",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "twice", description: "one", parameters: { type: "object" }, run: () => "one" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "second",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "twice", description: "two", parameters: { type: "object" }, run: () => "two" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "bad",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "has space", description: "bad", parameters: { type: "object" }, run: () => "bad" }); }"#,
        )],
    )
    .await;
    let (plugins, notes) = loaded_with_notes(root.path()).await;
    assert_eq!(names(&plugins), ["twice"]);
    assert_eq!(&*run(&plugins, "twice", "{}").await.content, "one");
    assert!(
        notes.iter().any(|note| note.starts_with("plugin second: ")
            && note.contains("duplicate tool name 'twice'")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin bad: ") && note.contains("has space")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_plugin_that_throws_registers_nothing() {
    let root = temp_root();
    plugin(
        root.path(),
        "a_good",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerTool({ name: "kept", description: "kept", parameters: { type: "object" }, run: () => "kept" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "b_half",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({ name: "gone", description: "gone", parameters: { type: "object" }, run: () => "gone" });
  r.hook("tool.call", () => ({ block: "no" }));
  r.hook("tool.result", () => ({ text: "changed" }));
  r.hook("turn.end", () => "again");
  r.on("turn_end", () => { throw new Error("listened"); });
  throw new Error("load trouble");
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(names(&plugins), ["kept"]);
    assert!(!plugins.call, "no tool.call layer survived the throw");
    assert!(!plugins.result, "no tool.result layer survived the throw");
    assert!(!plugins.turn, "no turn.end layer survived the throw");
    assert_eq!(plugins.events, 0, "no listener survived the throw");
    assert_eq!(plugins.before("kept", "{}").await, CallVerdict::Keep);
    assert_eq!(
        &*plugins
            .after("kept", "{}", ToolOutput::ok("hi"))
            .await
            .content,
        "hi"
    );
}

#[tokio::test]
async fn no_hook_answers_without_the_thread() {
    let root = temp_root();
    plugin(
        root.path(),
        "spin",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "spin",
    description: "keeps the thread busy",
    parameters: { type: "object" },
    run() {
      const until = Date.now() + 300;
      while (Date.now() < until) {}
      return "spun";
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert!(!plugins.call, "no plugin installed a tool.call layer");
    let spinning = plugins.clone();
    let busy = tokio::spawn(async move { run(&spinning, "spin", "{}").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    for _ in 0..3 {
        assert_eq!(
            plugins.before("shout", r#"{"text":"hi"}"#).await,
            CallVerdict::Keep
        );
    }
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(100),
        "it waited on the busy thread: {took:?}"
    );
    assert_eq!(&*busy.await.unwrap().content, "spun");
}

/// The panel with this id, from the panels the host sees.
fn panel<'a>(panels: &'a [PanelView], id: &str) -> &'a PanelView {
    panels
        .iter()
        .find(|panel| panel.id == id)
        .unwrap_or_else(|| panic!("no panel {id}: {panels:?}"))
}

/// A panel's lines as JSON.
fn lines(view: &PanelView) -> serde_json::Value {
    serde_json::from_str(&view.lines)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", view.lines))
}

#[tokio::test]
async fn a_command_is_listed_with_its_description() {
    let root = temp_root();
    plugin(
        root.path(),
        "echoer",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("echo", { description: "Says it back.", run: (args) => `got: ${args}` });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.commands().to_vec(),
        [CommandInfo {
            name: "echo".into(),
            description: "Says it back.".into(),
        }]
    );
}

#[tokio::test]
async fn a_duplicate_command_name_is_refused() {
    let root = temp_root();
    plugin(
        root.path(),
        "first",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("twice", { description: "one", run: () => "one" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "second",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("twice", { description: "two", run: () => "two" }); }"#,
        )],
    )
    .await;
    let (plugins, notes) = loaded_with_notes(root.path()).await;
    assert_eq!(
        plugins
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["twice"]
    );
    assert_eq!(plugins.command("twice", "").await.as_deref(), Some("one"));
    assert!(
        notes.iter().any(|note| note.starts_with("plugin second: ")
            && note.contains("duplicate command name 'twice'")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_bad_command_name_fails_the_load() {
    let root = temp_root();
    plugin(
        root.path(),
        "a_good",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("kept", { description: "kept", run: () => "kept" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "b_bad",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("has space", { description: "bad", run: () => "bad" }); }"#,
        )],
    )
    .await;
    let (plugins, notes) = loaded_with_notes(root.path()).await;
    assert_eq!(
        plugins
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["kept"]
    );
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin b_bad: ") && note.contains("has space")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_command_gets_the_rest_of_the_line() {
    let root = temp_root();
    plugin(
        root.path(),
        "echoer",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("echo", { description: "echoes", run: (args) => `got: ${args}` }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.command("echo", "a b  c").await.as_deref(),
        Some("got: a b  c")
    );
}

#[tokio::test]
async fn a_command_that_returns_nothing_shows_nothing() {
    let root = temp_root();
    plugin(
        root.path(),
        "quiet",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("quiet", { description: "quiet", run: () => undefined }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(plugins.command("quiet", "anything").await, None);
}

#[tokio::test]
async fn an_async_command_answers() {
    let root = temp_root();
    plugin(
        root.path(),
        "late",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("later", {
    description: "answers after a microtask",
    async run(args) { return await Promise.resolve(`late: ${args}`); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.command("later", "x").await.as_deref(),
        Some("late: x")
    );
}

#[tokio::test]
async fn a_command_that_throws_names_itself() {
    let root = temp_root();
    plugin(
        root.path(),
        "angry",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("boom", { description: "throws", run: () => { throw new Error("no way"); } }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let text = plugins
        .command("boom", "")
        .await
        .unwrap_or_else(|| panic!("a throw is a text"));
    assert!(text.contains("angry"), "{text}");
    assert!(text.contains("boom"), "{text}");
    assert!(text.contains("no way"), "{text}");
}

#[tokio::test]
async fn a_command_that_loops_is_stopped_and_the_next_runs() {
    let root = temp_root();
    plugin(
        root.path(),
        "hang",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("stuck", { description: "stuck", run: () => { while (true) {} } });
  r.registerCommand("next", { description: "next", run: () => "ran" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let text = plugins
        .command("stuck", "")
        .await
        .unwrap_or_else(|| panic!("a stop is a text"));
    let took = started.elapsed();
    assert!(text.contains("hang"), "{text}");
    assert!(text.contains("stuck"), "{text}");
    assert!(took < Duration::from_secs(6), "it took {took:?}");
    // The thread survived the stop.
    let started = Instant::now();
    assert_eq!(plugins.command("next", "").await.as_deref(), Some("ran"));
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "the second command took {took:?}"
    );
}

#[tokio::test]
async fn an_unknown_command_gives_a_text() {
    let root = temp_root();
    plugin(
        root.path(),
        "echoer",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("echo", { description: "echoes", run: (args) => args }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let text = plugins
        .command("missing", "")
        .await
        .unwrap_or_else(|| panic!("an unknown command is a text"));
    assert!(text.contains("missing"), "{text}");
}

#[tokio::test]
async fn a_panel_lists_its_lines() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.ui.panel({ id: "build", title: "Build" }).set([
    "ok",
    { text: "fail", role: "bad" },
    { key: "tests", value: 12 },
    { label: "done", value: 3, max: 4 },
  ]);
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let panels = plugins.panels();
    let view = panel(&panels, "panelmaker/build");
    assert_eq!(view.title, "Build");
    assert_eq!(
        lines(view),
        json!([
            "ok",
            { "text": "fail", "role": "bad" },
            { "key": "tests", "value": "12" },
            { "label": "done", "value": 3, "max": 4 }
        ])
    );
}

#[tokio::test]
async fn a_panel_drops_an_unknown_role() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.ui.panel({ id: "roles", title: "Roles" }).set([{ text: "hi", role: "purple" }, { text: "bye", role: "good" }]); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let panels = plugins.panels();
    assert_eq!(
        lines(panel(&panels, "panelmaker/roles")),
        json!([{ "text": "hi" }, { "text": "bye", "role": "good" }])
    );
}

#[tokio::test]
async fn a_panel_keeps_fifty_lines() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  const p = r.ui.panel({ id: "many", title: "Many" });
  r.registerCommand("draw", {
    description: "draws eighty lines",
    run() { p.set(Array.from({ length: 80 }, (_, at) => String(at))); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("draw", "").await;
    let panels = plugins.panels();
    let kept = lines(panel(&panels, "panelmaker/many"));
    let kept = kept.as_array().unwrap_or_else(|| panic!("not an array"));
    assert_eq!(kept.len(), 50);
    assert_eq!(kept[0], json!("0"));
    assert_eq!(kept[49], json!("49"));
}

#[tokio::test]
async fn a_panel_skips_a_bad_line_with_a_note() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  const p = r.ui.panel({ id: "bad", title: "Bad" });
  r.registerCommand("draw", {
    description: "draws a bad line",
    run() { p.set(["ok", 42, null, { nope: true }]); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("draw", "").await;
    let panels = plugins.panels();
    assert_eq!(lines(panel(&panels, "panelmaker/bad")), json!(["ok"]));
    let notes = plugins.take_notes();
    assert_eq!(notes.len(), 1, "one note per set: {notes:?}");
    assert!(notes[0].contains("panelmaker/bad"), "{notes:?}");
    assert!(notes[0].contains("3 bad lines"), "{notes:?}");
}

#[tokio::test]
async fn a_panel_needs_an_array() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  const p = r.ui.panel({ id: "pick", title: "Pick" });
  r.registerCommand("draw", { description: "draws", run() { p.set("nope"); } });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let text = plugins
        .command("draw", "")
        .await
        .unwrap_or_else(|| panic!("a throw is a text"));
    assert!(text.contains("array"), "{text}");
}

#[tokio::test]
async fn panels_keep_creation_order_and_changes() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.ui.panel({ id: "one", title: "One" });
  const two = r.ui.panel({ id: "two", title: "Two" });
  r.registerCommand("draw", { description: "draws", run() { two.set(["drawn"]); } });
}
"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "other",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.ui.panel({ id: "one", title: "Other" }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let ids = |panels: Vec<PanelView>| {
        panels
            .into_iter()
            .map(|panel| panel.id)
            .collect::<Vec<String>>()
    };
    // The folders load in name order: `other` before `panelmaker`.
    assert_eq!(
        ids(plugins.panels()),
        ["other/one", "panelmaker/one", "panelmaker/two"]
    );
    assert!(
        plugins.take_changed_panels().is_empty(),
        "nothing changed yet"
    );
    plugins.command("draw", "").await;
    assert_eq!(ids(plugins.take_changed_panels()), ["panelmaker/two"]);
    assert!(plugins.take_changed_panels().is_empty(), "the marks clear");
}

#[tokio::test]
async fn a_panel_with_the_same_id_updates_its_title() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  const first = r.ui.panel({ id: "same", title: "A" });
  const second = r.ui.panel({ id: "same", title: "B" });
  r.registerCommand("draw", { description: "draws", run() { first.set(["from one"]); } });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("draw", "").await;
    let panels = plugins.panels();
    assert_eq!(panels.len(), 1, "{panels:?}");
    let view = panel(&panels, "panelmaker/same");
    assert_eq!(view.title, "B");
    assert_eq!(lines(view), json!(["from one"]));
}

#[tokio::test]
async fn a_burst_of_sets_keeps_the_last_lines() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  const p = r.ui.panel({ id: "count", title: "Count" });
  r.registerCommand("draw", {
    description: "draws a thousand times",
    run() { for (let at = 0; at < 1000; at += 1) p.set([String(at)]); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("draw", "").await;
    // A thousand sets wake the host once, not a thousand times.
    tokio::time::timeout(Duration::from_secs(5), plugins.changed())
        .await
        .expect("a burst of sets wakes changed");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), plugins.changed())
            .await
            .is_err(),
        "the burst woke the host a second time"
    );
    let changed = plugins.take_changed_panels();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(lines(&changed[0]), json!(["999"]));
    assert!(plugins.take_changed_panels().is_empty());
}

#[tokio::test]
async fn send_queues_texts_in_order() {
    let root = temp_root();
    plugin(
        root.path(),
        "talker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("talk", {
    description: "talks",
    run() { r.send("one"); r.send("two"); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("talk", "").await;
    assert_eq!(plugins.take_sent(), ["one", "two"]);
    assert!(plugins.take_sent().is_empty(), "the queue is taken once");
}

#[tokio::test]
async fn send_keeps_sixteen_and_notes_the_rest() {
    let root = temp_root();
    plugin(
        root.path(),
        "talker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("spam", {
    description: "spams",
    run() { for (let at = 0; at < 20; at += 1) r.send(`m${at}`); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("spam", "").await;
    let notes = plugins.take_notes();
    assert_eq!(
        notes,
        ["plugins: 16 messages were waiting, the rest were dropped"]
    );
    let sent = plugins.take_sent();
    assert_eq!(sent.len(), 16, "{sent:?}");
    assert_eq!(sent[0], "m0");
    assert_eq!(sent[15], "m15");
    assert!(plugins.take_notes().is_empty(), "one note per burst");
}

#[tokio::test]
async fn an_empty_send_is_ignored() {
    let root = temp_root();
    plugin(
        root.path(),
        "talker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("quiet", {
    description: "says nothing",
    run() { r.send(""); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("quiet", "").await;
    assert!(plugins.take_sent().is_empty());
    assert!(plugins.take_notes().is_empty());
}

#[tokio::test]
async fn a_sent_text_is_cut() {
    let root = temp_root();
    plugin(
        root.path(),
        "talker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("long", {
    description: "says too much",
    run() { r.send("x".repeat(40000)); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("long", "").await;
    let sent = plugins.take_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].len(), 32 * 1024);
}

#[tokio::test]
async fn changed_waits_when_nothing_happened() {
    let root = temp_root();
    plugin(
        root.path(),
        "idle",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  const p = r.ui.panel({ id: "idle", title: "Idle" });
  r.registerCommand("draw", { description: "draws", run() { p.set(["ok"]); } });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let waited = tokio::time::timeout(Duration::from_millis(200), plugins.changed()).await;
    assert!(waited.is_err(), "nothing changed");
}

#[tokio::test]
async fn changed_wakes_after_a_set() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  const p = r.ui.panel({ id: "build", title: "Build" });
  r.registerCommand("draw", { description: "draws", run() { p.set(["ok"]); } });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), plugins.changed())
            .await
            .is_err(),
        "nothing changed yet"
    );
    plugins.command("draw", "").await;
    tokio::time::timeout(Duration::from_secs(5), plugins.changed())
        .await
        .expect("a set wakes changed");
}

#[tokio::test]
async fn changed_wakes_after_a_send() {
    let root = temp_root();
    plugin(
        root.path(),
        "talker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("talk", { description: "talks", run() { r.send("hi"); } });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), plugins.changed())
            .await
            .is_err(),
        "nothing changed yet"
    );
    plugins.command("talk", "").await;
    tokio::time::timeout(Duration::from_secs(5), plugins.changed())
        .await
        .expect("a send wakes changed");
}

#[tokio::test]
async fn changed_wakes_after_a_note() {
    let root = temp_root();
    plugin(
        root.path(),
        "grumpy",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => { throw new Error("grumpy"); });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), plugins.changed())
            .await
            .is_err(),
        "nothing changed yet"
    );
    assert_eq!(plugins.before("shout", "{}").await, CallVerdict::Keep);
    tokio::time::timeout(Duration::from_secs(5), plugins.changed())
        .await
        .expect("a note wakes changed");
    assert!(!plugins.take_notes().is_empty(), "the throw raised a note");
}

#[tokio::test]
async fn a_plugin_that_throws_leaves_no_command_or_panel() {
    let root = temp_root();
    plugin(
        root.path(),
        "a_good",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("kept", { description: "kept", run: () => "kept" }); }"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "b_half",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("gone", { description: "gone", run: () => "gone" });
  r.ui.panel({ id: "gone", title: "Gone" }).set(["gone"]);
  throw new Error("load trouble");
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["kept"]
    );
    assert!(
        plugins.panels().is_empty(),
        "no panel survived the throw: {:?}",
        plugins.panels()
    );
    let text = plugins
        .command("gone", "")
        .await
        .unwrap_or_else(|| panic!("no such command"));
    // The exact unknown-command text: a command that returns `gone` is not it.
    assert_eq!(text, "plugins: no command is named '/gone'");
}

#[tokio::test]
async fn a_command_shows_any_other_value_as_text() {
    let root = temp_root();
    plugin(
        root.path(),
        "counter",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("count", { description: "counts", run: () => 42 }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(plugins.command("count", "").await.as_deref(), Some("42"));
}

/// A `package.json` with a fixed name, so two folders claim the same name.
fn same_name() -> &'static str {
    r#"{"name": "same", "version": "1.0.0", "type": "module", "reagent": {"entry": "index.js"}}"#
}

#[tokio::test]
async fn a_panel_made_after_load_reaches_the_host() {
    let root = temp_root();
    plugin(
        root.path(),
        "latecomer",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("late", {
    description: "draws a panel it makes now",
    run() { r.ui.panel({ id: "late", title: "Late" }).set(["x"]); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert!(plugins.panels().is_empty(), "nothing draws at load");
    plugins.command("late", "").await;
    tokio::time::timeout(Duration::from_secs(5), plugins.changed())
        .await
        .expect("a new panel wakes changed");
    let panels = plugins.panels();
    let view = panel(&panels, "latecomer/late");
    assert_eq!(view.title, "Late");
    assert_eq!(lines(view), json!(["x"]));
    let changed = plugins.take_changed_panels();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(changed[0].id, "latecomer/late");
    assert_eq!(lines(&changed[0]), json!(["x"]));
}

#[tokio::test]
async fn two_plugins_with_the_same_name_do_not_share_panels() {
    let root = temp_root();
    plugin(
        root.path(),
        "first",
        same_name(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.ui.panel({ id: "one", title: "First" });
  r.registerCommand("first_cmd", { description: "first", run: () => "first" });
}
"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "second",
        same_name(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.ui.panel({ id: "one", title: "Second" }).set(["second"]);
  r.registerCommand("second_cmd", { description: "second", run: () => "second" });
}
"#,
        )],
    )
    .await;
    let (plugins, notes) = loaded_with_notes(root.path()).await;
    let panels = plugins.panels();
    assert_eq!(panels.len(), 1, "{panels:?}");
    assert_eq!(panels[0].id, "same/one");
    assert_eq!(panels[0].title, "First");
    assert_eq!(
        lines(&panels[0]),
        json!([]),
        "the second plugin's set may not write into the first panel"
    );
    assert_eq!(
        plugins
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["first_cmd"]
    );
    let note = notes
        .iter()
        .find(|note| note.starts_with("plugin second: "))
        .unwrap_or_else(|| panic!("no note about the second plugin: {notes:?}"));
    assert!(note.contains("'same'"), "{note}");
    assert!(
        note.contains("first"),
        "the note names both folders: {note}"
    );
}

#[tokio::test]
async fn a_key_value_number_is_shown_as_js_strings_it() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.ui.panel({ id: "numbers", title: "Numbers" }).set([
    { key: "negative zero", value: -0 },
    { key: "tiny", value: 1e-7 },
    { key: "half", value: 1.5 },
    { key: "whole", value: 12 },
  ]);
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let panels = plugins.panels();
    assert_eq!(
        lines(panel(&panels, "panelmaker/numbers")),
        json!([
            { "key": "negative zero", "value": "0" },
            { "key": "tiny", "value": "1e-7" },
            { "key": "half", "value": "1.5" },
            { "key": "whole", "value": "12" }
        ])
    );
}

#[tokio::test]
async fn a_bar_value_is_clamped_to_a_whole() {
    let root = temp_root();
    plugin(
        root.path(),
        "panelmaker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.ui.panel({ id: "bars", title: "Bars" }).set([
    { label: "big", value: Infinity, max: Infinity },
    { label: "none", value: NaN, max: NaN },
    { label: "below", value: -3, max: -1 },
    { label: "part", value: 2.9, max: 4.9 },
  ]);
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let panels = plugins.panels();
    assert_eq!(
        lines(panel(&panels, "panelmaker/bars")),
        json!([
            { "label": "big", "value": u64::MAX, "max": u64::MAX },
            { "label": "none", "value": 0, "max": 0 },
            { "label": "below", "value": 0, "max": 0 },
            { "label": "part", "value": 2, "max": 4 }
        ])
    );
}

#[tokio::test]
async fn a_command_that_returns_null_shows_nothing() {
    let root = temp_root();
    plugin(
        root.path(),
        "quiet",
        manifest(),
        &[(
            "index.js",
            r#"export default function (r) { r.registerCommand("quiet", { description: "quiet", run: () => null }); }"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(plugins.command("quiet", "").await, None);
}

#[tokio::test]
async fn a_dropped_send_raises_its_note_without_take_sent() {
    let root = temp_root();
    plugin(
        root.path(),
        "talker",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("spam", {
    description: "spams",
    run() { for (let at = 0; at < 20; at += 1) r.send(`m${at}`); },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    plugins.command("spam", "").await;
    // A host that only drains notes still learns of the drops.
    let notes = plugins.take_notes();
    assert_eq!(
        notes,
        ["plugins: 16 messages were waiting, the rest were dropped"]
    );
    assert_eq!(plugins.take_sent().len(), 16, "the queue keeps its sixteen");
    assert!(plugins.take_notes().is_empty(), "one note per burst");
    // The next burst has its own note.
    plugins.command("spam", "").await;
    assert_eq!(plugins.take_notes().len(), 1, "one note per burst");
}
