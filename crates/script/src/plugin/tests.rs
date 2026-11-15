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

use super::{CommandInfo, PanelView, Plugins, lock};

/// A term for the turn view the turn hook reads.
const NO_TOOLS: &[(reagent_ai::ToolCall, reagent_ai::ToolResult)] = &[];

/// The longest a test waits for a plugin to reach a state, so a loaded machine
/// slows a test down instead of failing it.
const WAIT: Duration = Duration::from_secs(5);

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

/// The `package.json` of a plugin that may install a `tool.call` layer.
fn call_manifest() -> &'static str {
    r#"{"name": "__NAME__", "version": "1.0.0", "type": "module", "reagent": {"entry": "index.js", "permissions": {"hooks": ["tool.call"]}}}"#
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

/// Loads the plugins under `root`, which must have loaded some, with the notes
/// the load raised.
async fn loaded_with_notes(root: &Path) -> (std::sync::Arc<Plugins>, Vec<String>) {
    let (plugins, notes) = Plugins::load(root, &state(), None).await;
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
    let (none, notes) = Plugins::load(root.path(), &state(), None).await;
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
    let (none, notes) = Plugins::load(root.path(), &state(), None).await;
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
    let (none, notes) = Plugins::load(second.path(), &state(), None).await;
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
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
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
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
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
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin abs: ")
                && note.contains(shared.to_str().unwrap())),
        "{notes:?}"
    );
}

/// The entry of a plugin that imports the bare name `tiny` and answers what it
/// exports.
const LOCKED_ENTRY: &str = r#"
import { tiny } from "tiny";
export default function (reagent) {
  reagent.registerTool({
    name: "tinied",
    description: "Answers what the tiny module exports.",
    parameters: { type: "object" },
    run: () => tiny,
  });
}
"#;

/// The module a lock maps the bare name `tiny` to.
const TINY: &str = "export const tiny = \"tiny-value\";\n";

/// Writes the plugin `locked`, whose entry imports the bare name `tiny` from
/// `deps/tiny/index.mjs` holding `module`, and answers its folder.
async fn tiny_plugin(root: &Path, module: &str) -> PathBuf {
    plugin(
        root,
        "locked",
        manifest(),
        &[("index.js", LOCKED_ENTRY), ("deps/tiny/index.mjs", module)],
    )
    .await;
    root.join("locked")
}

/// Writes `reagent-lock.json` into the plugin folder `dir`: `files` are the
/// paths under the folder with their bytes, and `imports` its bare import map.
async fn lockfile(dir: &Path, files: &[(&str, &str)], imports: &[(&str, &str)]) {
    let lock = lock::Lock {
        version: lock::VERSION,
        requested: "npm:tiny@^1".to_owned(),
        source: "npm:tiny@1.0.0".to_owned(),
        commit: None,
        files: files
            .iter()
            .map(|(path, body)| ((*path).to_owned(), lock::hash(body.as_bytes())))
            .collect(),
        imports: imports
            .iter()
            .map(|(name, path)| ((*name).to_owned(), (*path).to_owned()))
            .collect(),
    };
    // The install writes it the same way, off the async runtime.
    tokio::task::spawn_blocking({
        let dir = dir.to_path_buf();
        move || lock::write(&dir, &lock)
    })
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn lockfile_staging_folder_does_not_load() {
    // An install stages the plugin under a name that starts with `.` until the
    // user says yes, and an update parks the old one under such a name: a run
    // that starts meanwhile must load neither.
    let root = temp_root();
    let dir = tiny_plugin(root.path(), TINY).await;
    lockfile(
        &dir,
        &[("index.js", LOCKED_ENTRY), ("deps/tiny/index.mjs", TINY)],
        &[("tiny", "deps/tiny/index.mjs")],
    )
    .await;
    tokio::fs::rename(&dir, root.path().join(".staging-locked-42"))
        .await
        .unwrap();
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "a staged plugin loaded");
    assert!(notes.is_empty(), "{notes:?}");
}

#[tokio::test]
async fn lockfile_imports_load_the_pinned_file() {
    let root = temp_root();
    let dir = tiny_plugin(root.path(), TINY).await;
    lockfile(
        &dir,
        &[("index.js", LOCKED_ENTRY), ("deps/tiny/index.mjs", TINY)],
        &[("tiny", "deps/tiny/index.mjs")],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(names(&plugins), ["tinied"]);
    let out = run(&plugins, "tinied", "{}").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(&*out.content, "tiny-value");
}

#[tokio::test]
async fn lockfile_names_a_changed_file_and_the_plugin_does_not_load() {
    let root = temp_root();
    let dir = tiny_plugin(root.path(), TINY).await;
    lockfile(
        &dir,
        &[("index.js", LOCKED_ENTRY), ("deps/tiny/index.mjs", TINY)],
        &[("tiny", "deps/tiny/index.mjs")],
    )
    .await;
    tokio::fs::write(
        dir.join("deps/tiny/index.mjs"),
        "export const tiny = \"changed\";\n",
    )
    .await
    .unwrap();
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin locked: ")
                && note.contains("deps/tiny/index.mjs")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn lockfile_that_does_not_pin_the_entry_does_not_load() {
    let root = temp_root();
    let dir = tiny_plugin(root.path(), TINY).await;
    lockfile(
        &dir,
        &[("deps/tiny/index.mjs", TINY)],
        &[("tiny", "deps/tiny/index.mjs")],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin locked: ") && note.contains("index.js")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn lockfile_refuses_a_relative_import_it_does_not_pin() {
    let root = temp_root();
    let entry = r#"
import { extra } from "./extra.js";
export default function (reagent) {
  reagent.registerTool({
    name: "extraed",
    description: "Answers what the extra module exports.",
    parameters: { type: "object" },
    run: () => extra,
  });
}
"#;
    let extra = "export const extra = \"extra-value\";\n";
    plugin(
        root.path(),
        "locked",
        manifest(),
        &[("index.js", entry), ("extra.js", extra)],
    )
    .await;
    lockfile(&root.path().join("locked"), &[("index.js", entry)], &[]).await;
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes.iter().any(|note| note.starts_with("plugin locked: ")
            && note.contains("./extra.js")
            && note.contains("not in the plugin's lockfile")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn lockfile_absent_keeps_the_old_bare_import_error() {
    let root = temp_root();
    plugin(
        root.path(),
        "nolock",
        manifest(),
        &[("index.js", LOCKED_ENTRY), ("deps/tiny/index.mjs", TINY)],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes.iter().any(|note| note.starts_with("plugin nolock: ")
            && note.contains("cannot import 'tiny': only relative paths inside the plugin folder")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn lockfile_that_is_malformed_skips_the_plugin() {
    let root = temp_root();
    let dir = tiny_plugin(root.path(), TINY).await;
    tokio::fs::write(dir.join(lock::FILE), "{not json\n")
        .await
        .unwrap();
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin locked: ") && note.contains(lock::FILE)),
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
        call_manifest(),
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
        call_manifest(),
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
        call_manifest(),
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
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
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
        call_manifest(),
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
        call_manifest(),
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
        call_manifest(),
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

#[tokio::test]
async fn async_tool_promise() {
    let root = temp_root();
    plugin(
        root.path(),
        "promiser",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "now",
    description: "resolves at once",
    parameters: { type: "object" },
    run: () => Promise.resolve("hi"),
  });
  r.registerTool({
    name: "later",
    description: "resolves on a timer",
    parameters: { type: "object" },
    run: () => new Promise((resolve) => setTimeout(() => resolve("later"), 50)),
  });
  r.registerTool({
    name: "micro",
    description: "resolves in a microtask",
    parameters: { type: "object" },
    run: () => new Promise((resolve) => queueMicrotask(() => resolve("micro"))),
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    for (tool, text) in [("now", "hi"), ("later", "later"), ("micro", "micro")] {
        let out = run(&plugins, tool, "{}").await;
        assert!(!out.is_error, "{tool}: {}", out.content);
        assert_eq!(&*out.content, text, "the {tool} promise");
    }
}

#[tokio::test]
async fn async_tool_rejects() {
    let root = temp_root();
    plugin(
        root.path(),
        "refuser",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "no",
    description: "rejects",
    parameters: { type: "object" },
    run: () => Promise.reject(new Error("boom")),
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let out = run(&plugins, "no", "{}").await;
    assert!(out.is_error, "a rejection is an error result");
    assert!(out.content.contains("boom"), "{}", out.content);
}

#[tokio::test]
async fn async_hook_never_settles() {
    let root = temp_root();
    plugin(
        root.path(),
        "hangs",
        call_manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => new Promise(() => {}));
  r.registerCommand("fine", { description: "answers", run: () => "fine" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let verdict = plugins.before("shout", "{}").await;
    let took = started.elapsed();
    assert_eq!(verdict, CallVerdict::Keep, "the call goes on unchanged");
    assert!(took >= Duration::from_secs(4), "it took {took:?}");
    assert!(took < Duration::from_secs(6), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin hangs: hook tool.call stopped after 5 s"),
        "{notes:?}"
    );
    // A promise nothing settles does not take the thread with it.
    assert_eq!(plugins.command("fine", "").await.as_deref(), Some("fine"));
    assert!(plugins.take_notes().is_empty(), "the answer raised no note");
}

#[tokio::test]
async fn async_command_promise() {
    let root = temp_root();
    plugin(
        root.path(),
        "promiser",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("later", {
    description: "answers on a timer",
    run: () => new Promise((resolve) => setTimeout(() => resolve("later"), 20)),
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(plugins.command("later", "").await.as_deref(), Some("later"));
}

#[tokio::test]
async fn async_timer_panel() {
    let root = temp_root();
    plugin(
        root.path(),
        "latecomer",
        manifest(),
        &[(
            "index.js",
            r#"
let panel;
export default function (r) {
  panel = r.ui.panel({ id: "late", title: "Late" });
  setTimeout(() => panel.set(["from the timer"]), 100);
}
"#,
        )],
    )
    .await;
    // No job is sent: the timer alone must wake the host.
    let plugins = loaded(root.path()).await;
    let arrived = async {
        loop {
            let panels = plugins.panels();
            if panels
                .iter()
                .any(|view| lines(view) == json!(["from the timer"]))
            {
                return;
            }
            plugins.changed().await;
        }
    };
    tokio::time::timeout(WAIT, arrived)
        .await
        .expect("a panel set in a timer callback wakes changed");
    let panels = plugins.panels();
    assert_eq!(
        lines(panel(&panels, "latecomer/late")),
        json!(["from the timer"])
    );
    let changed = plugins.take_changed_panels();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(changed[0].id, "latecomer/late");
}

#[tokio::test]
async fn async_interval_cleared() {
    let root = temp_root();
    plugin(
        root.path(),
        "ticker",
        manifest(),
        &[(
            "index.js",
            r#"
let runs = 0;
export default function (r) {
  r.registerCommand("runs", { description: "how many runs", run: () => String(runs) });
  const id = setInterval(() => {
    runs += 1;
    if (runs === 3) clearInterval(id);
  }, 20);
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    // The interval clears itself on its third run, so the count must reach 3.
    let started = Instant::now();
    let mut runs = String::new();
    while runs != "3" {
        runs = plugins.command("runs", "").await.unwrap_or_default();
        assert!(
            started.elapsed() < WAIT,
            "the interval ran {runs} times in {} s",
            WAIT.as_secs()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Cleared, it runs no more: a further wait leaves the count at three.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(plugins.command("runs", "").await.as_deref(), Some("3"));
}

#[tokio::test]
async fn async_timer_limit() {
    let root = temp_root();
    plugin(
        root.path(),
        "flooder",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("flood", {
    description: "sets one timer too many",
    run: () => {
      let name = "none";
      try {
        for (let at = 0; at < 1024; at += 1) setTimeout(() => {}, 10000);
        setTimeout(() => {}, 10000);
      } catch (error) {
        name = error.name;
      }
      return name;
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.command("flood", "").await.as_deref(),
        Some("RangeError")
    );
}

#[tokio::test]
async fn async_timer_throws() {
    let root = temp_root();
    plugin(
        root.path(),
        "thrower",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "works",
    description: "still works",
    parameters: { type: "object" },
    run: () => "fine",
  });
  setTimeout(() => { throw new Error("from the timer"); }, 10);
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let raised = async {
        loop {
            let notes = plugins.take_notes();
            if !notes.is_empty() {
                return notes;
            }
            plugins.changed().await;
        }
    };
    let notes = tokio::time::timeout(WAIT, raised)
        .await
        .expect("a throw in a timer callback raises a note");
    assert_eq!(notes, ["plugins: a timer callback threw: from the timer"]);
    let out = run(&plugins, "works", "{}").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(&*out.content, "fine");
}

#[tokio::test]
async fn async_spin_in_timer() {
    let root = temp_root();
    plugin(
        root.path(),
        "spinner",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "after",
    description: "runs after the timer was stopped",
    parameters: { type: "object" },
    run: () => "after",
  });
  setTimeout(() => { for (;;) {} }, 1);
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    // The timer callback spins; its own deadline stops it, and the note says so.
    tokio::time::timeout(Duration::from_secs(7), plugins.changed())
        .await
        .expect("a spinning timer callback is stopped");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugins: a timer callback stopped after 5 s"),
        "{notes:?}"
    );
    // The thread survived the stop: the next tool call answers at once.
    let started = Instant::now();
    let out = run(&plugins, "after", "{}").await;
    let took = started.elapsed();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(&*out.content, "after");
    assert!(took < Duration::from_secs(1), "it took {took:?}");
}

#[tokio::test]
async fn async_timer_not_a_function() {
    let root = temp_root();
    plugin(
        root.path(),
        "stringer",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("bad", {
    description: "hands a string to a timer",
    run: () => {
      let name = "none";
      try {
        setTimeout("not a function", 1);
      } catch (error) {
        name = error.name;
      }
      // An id no timer took is nothing, not an error.
      clearTimeout(9999);
      clearInterval(9999);
      queueMicrotask(() => {});
      return name;
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.command("bad", "").await.as_deref(),
        Some("TypeError")
    );
}

/// Loads the plugins under `root` that answer `/spin` with `"x"` after queueing
/// a job that never returns, and answer `/fine` at once.
async fn spinner(root: &Path, queue: &str) {
    let index = format!(
        r#"
export default function (r) {{
  r.registerCommand("spin", {{
    description: "queues a job that never returns",
    run: () => {{
      {queue}
      return "x";
    }},
  }});
  r.registerCommand("fine", {{ description: "answers", run: () => "fine" }});
}}
"#
    );
    plugin(root, "spinner", manifest(), &[("index.js", &index)]).await;
}

/// Runs `/spin`, then expects the stopped-job note and an answer to `/fine`.
async fn stopped_job_keeps_the_thread(plugins: &Plugins) {
    assert_eq!(plugins.command("spin", "").await.as_deref(), Some("x"));
    // The job runs while the thread waits for its next job: a window of the
    // idle wait is what stops it.
    let raised = async {
        loop {
            let notes = plugins.take_notes();
            if !notes.is_empty() {
                return notes;
            }
            plugins.changed().await;
        }
    };
    let notes = tokio::time::timeout(Duration::from_secs(7), raised)
        .await
        .expect("a job that spins is stopped");
    assert_eq!(notes, ["plugins: a promise job stopped after 5 s"]);
    let started = Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(8), plugins.command("fine", ""))
        .await
        .expect("the thread still answers");
    assert_eq!(out.as_deref(), Some("fine"));
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[tokio::test]
async fn async_spin_in_microtask() {
    let root = temp_root();
    spinner(root.path(), "queueMicrotask(() => { for (;;) {} });").await;
    let plugins = loaded(root.path()).await;
    stopped_job_keeps_the_thread(&plugins).await;
}

#[tokio::test]
async fn async_spin_in_then() {
    let root = temp_root();
    spinner(
        root.path(),
        "Promise.resolve().then(() => { for (;;) {} });",
    )
    .await;
    let plugins = loaded(root.path()).await;
    stopped_job_keeps_the_thread(&plugins).await;
}

#[tokio::test]
async fn async_cleared_timers_free_their_futures() {
    let root = temp_root();
    plugin(
        root.path(),
        "churn",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("churn", {
    description: "sets and clears a hundred thousand long timers",
    run: async () => {
      // One call may open at most 16384 timer futures, so each round waits for
      // the scheduler to drop the futures its clears ended.
      for (let round = 0; round < 10; round += 1) {
        for (let at = 0; at < 10000; at += 1) clearTimeout(setTimeout(() => {}, 2e9));
        await new Promise((resolve) => setTimeout(resolve, 0));
      }
      return "churned";
    },
  });
  r.registerCommand("live", {
    description: "how many timer futures still wait",
    run: () => String(__liveTimers()),
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.command("churn", "").await.as_deref(),
        Some("churned")
    );
    // A cleared timer ends its future at once, so its sleep, its callback and
    // its arguments are gone rather than waiting out the two billion ms.
    let started = Instant::now();
    loop {
        let live = plugins.command("live", "").await.unwrap_or_default();
        if live == "0" {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{live} timer futures still wait 1 s after they were cleared"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn async_timer_limit_frees_slots() {
    let root = temp_root();
    plugin(
        root.path(),
        "reuser",
        manifest(),
        &[(
            "index.js",
            r#"
const fired = () => new Promise((resolve) => {
  for (let at = 0; at < 1024; at += 1) setTimeout(() => { if (at === 1023) resolve(); }, 1);
});
export default function (r) {
  r.registerCommand("fire", {
    description: "sets 1024 timers that fire, then 1024 more",
    run: async () => {
      await fired();
      await fired();
      return "fired";
    },
  });
  r.registerCommand("clear", {
    description: "sets and clears 1024 timers, then sets 1024 more",
    run: async () => {
      for (let at = 0; at < 1024; at += 1) clearTimeout(setTimeout(() => {}, 10000));
      await fired();
      return "cleared";
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(plugins.command("fire", "").await.as_deref(), Some("fired"));
    assert_eq!(
        plugins.command("clear", "").await.as_deref(),
        Some("cleared")
    );
}

#[tokio::test]
async fn async_deadline_counts_from_the_call() {
    let root = temp_root();
    plugin(
        root.path(),
        "latecomer",
        call_manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => {
    const end = Date.now() + 3000;
    while (Date.now() < end) {}
    return new Promise(() => {});
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let verdict = tokio::time::timeout(Duration::from_secs(9), plugins.before("shout", "{}"))
        .await
        .expect("the hook answers");
    let took = started.elapsed();
    assert_eq!(verdict, CallVerdict::Keep, "the call goes on unchanged");
    // The promise's deadline counts from the call, not from its return.
    assert!(
        took < Duration::from_secs(6),
        "the deadline started after the spin: {took:?}"
    );
    assert!(
        took >= Duration::from_secs(4),
        "it stopped too early: {took:?}"
    );
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin latecomer: hook tool.call stopped after 5 s"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn async_overlapping_windows() {
    let root = temp_root();
    plugin(
        root.path(),
        "overlapper",
        manifest(),
        &[(
            "index.js",
            r#"
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const burn = (ms) => { const end = Date.now() + ms; while (Date.now() < end) {} };
export default function (r) {
  r.registerTool({
    name: "slow",
    description: "waits past a second while a timer window opens and closes inside its own",
    parameters: { type: "object" },
    run: async () => {
      // A window that begins after this one and ends after it too, so the two
      // overlap without nesting: only an id tells them apart.
      setTimeout(async () => { await sleep(100); r.send("chain done"); }, 1250);
      await sleep(1300);
      // The interrupt handler runs in here, and marks this window slow.
      burn(20);
      return "slow done";
    },
  });
  r.registerTool({
    name: "fast",
    description: "answers at once",
    parameters: { type: "object" },
    run: () => "fast done",
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let out = run(&plugins, "slow", "{}").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        &*out.content, "slow done",
        "an intervening window stopped it"
    );
    // Wait for the timer window that overlapped the tool's own to end, so a
    // note it raised would be here.
    let started = Instant::now();
    let mut sent = plugins.take_sent();
    while sent.is_empty() && started.elapsed() < WAIT {
        tokio::time::sleep(Duration::from_millis(10)).await;
        sent = plugins.take_sent();
    }
    assert_eq!(sent, ["chain done"]);
    // The slow note names the tool call, whose window passed the second; the
    // timer window that overlapped it is no window's answer.
    assert_eq!(
        plugins.take_notes(),
        ["plugin overlapper: tool slow is slow"]
    );
    // The thread is still whole: the next call answers at once.
    let out = run(&plugins, "fast", "{}").await;
    assert_eq!(&*out.content, "fast done");
}

#[tokio::test]
async fn async_own_deadline() {
    let root = temp_root();
    plugin(
        root.path(),
        "owl",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("start", {
    description: "opens a timer window that waits four seconds inside it",
    run: () => {
      setTimeout(async () => {
        await new Promise((resolve) => setTimeout(resolve, 4500));
      }, 10);
      return "started";
    },
  });
  r.registerCommand("wait", {
    description: "answers through a promise three seconds after it is called",
    run: () => new Promise((resolve) => setTimeout(() => resolve("waited"), 3000)),
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.command("start", "").await.as_deref(),
        Some("started")
    );
    // The timer's window opened first and so does its deadline come first, but
    // the wait has five seconds of its own.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let started = Instant::now();
    let answer = tokio::time::timeout(Duration::from_secs(8), plugins.command("wait", ""))
        .await
        .expect("the wait answers");
    assert_eq!(
        answer.as_deref(),
        Some("waited"),
        "another window's deadline cut it short after {:?}",
        started.elapsed()
    );
    let notes = plugins.take_notes();
    assert!(
        !notes.iter().any(|note| note.contains("stopped")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn async_set_clear_loop_is_bounded() {
    let root = temp_root();
    plugin(
        root.path(),
        "churner",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerCommand("churn", {
    description: "sets and clears timers until the running futures run out",
    run: () => {
      let highest = 0;
      let name = "none";
      for (let at = 0; at < 100000; at += 1) {
        highest = Math.max(highest, __liveTimers());
        try {
          clearTimeout(setTimeout(() => {}, 2e9));
        } catch (error) {
          name = error.name;
          break;
        }
      }
      return `${name} ${highest} ${__liveTimers()}`;
    },
  });
  r.registerCommand("again", {
    description: "sets 1024 timers once the churn is over",
    run: () => {
      for (let at = 0; at < 1024; at += 1) setTimeout(() => {}, 2e9);
      return String(__liveTimers());
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let answer = plugins.command("churn", "").await.unwrap_or_default();
    let mut parts = answer.split(' ');
    assert_eq!(parts.next(), Some("RangeError"), "the loop ends: {answer}");
    let highest = parts.next().unwrap_or_default();
    let highest: usize = highest.parse().unwrap_or_default();
    assert!(
        highest <= 16384,
        "the loop ran {highest} futures before the cap"
    );
    assert!(
        highest >= 16000,
        "the loop stopped before the cap: {highest}"
    );
    // The futures ended when the call did, so the timers that are waiting are
    // the only ones left.
    assert_eq!(plugins.command("again", "").await.as_deref(), Some("1024"));
}

#[cfg(not(feature = "node"))]
#[tokio::test]
async fn no_node_module_is_refused_without_the_feature() {
    let root = temp_root();
    plugin(
        root.path(),
        "wants_node",
        manifest(),
        &[(
            "index.js",
            r#"import { join } from "node:path"; export default function (r) { r.registerTool({ name: "joins", description: "joins", parameters: { type: "object" }, run: () => join("a", "b") }); }"#,
        )],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin wants_node: ")
                && note.contains("node:path")
                && note.contains("no Node modules")),
        "{notes:?}"
    );
}

#[cfg(not(feature = "node"))]
#[tokio::test]
async fn no_node_bare_module_is_refused_without_the_feature() {
    let root = temp_root();
    plugin(
        root.path(),
        "wants_fs",
        manifest(),
        &[(
            "index.js",
            r#"import { readFileSync } from "fs"; export default function (r) { r.registerTool({ name: "reads", description: "reads", parameters: { type: "object" }, run: () => readFileSync("a.txt", "utf8") }); }"#,
        )],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin wants_fs: ")
                && note.contains("fs")
                && note.contains("no Node modules")),
        "{notes:?}"
    );
}

#[cfg(not(feature = "node"))]
#[tokio::test]
async fn no_node_child() {
    let root = temp_root();
    plugin(
        root.path(),
        "wants_child",
        manifest(),
        &[(
            "index.js",
            r#"import { spawn } from "node:child_process"; export default function (r) { r.registerTool({ name: "runs", description: "runs", parameters: { type: "object" }, run: () => typeof spawn }); }"#,
        )],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin wants_child: ")
                && note.contains("child_process")
                && note.contains("no Node modules")),
        "{notes:?}"
    );
}

#[cfg(not(feature = "node"))]
#[tokio::test]
async fn no_node_fetch() {
    let root = temp_root();
    plugin(
        root.path(),
        "wants_fetch",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (reagent) {
  reagent.registerTool({
    name: "looks",
    description: "Looks the fetch globals up.",
    parameters: { type: "object" },
    run() {
      return `${typeof fetch} ${typeof Request} ${typeof Response} ${typeof Headers}`;
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        &*run(&plugins, "looks", "{}").await.content,
        "undefined undefined undefined undefined"
    );
}

/// A plugin with one tool, one command and one `tool.call` hook, each of which
/// may `await` for a while: `tool`, `command` and `hook` are the bodies.
fn waits(tool: &str, command: &str, hook: &str) -> String {
    format!(
        r#"
export default function (reagent) {{
  reagent.hook("tool.call", async () => {{ {hook} }});
  reagent.registerTool({{
    name: "slow",
    description: "Waits.",
    parameters: {{ type: "object" }},
    async run() {{ {tool} }}
  }});
  reagent.registerCommand("slow", {{ description: "Waits.", async run() {{ {command} }} }});
}}
"#
    )
}

/// A promise that settles after `ms`.
const SLEEP: &str = "await new Promise((done) => setTimeout(done, 8000));";

#[tokio::test]
async fn async_long_tool_waits() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow_tool",
        call_manifest(),
        &[(
            "index.js",
            &waits(
                &format!(r#"{SLEEP} return "after 8 s";"#),
                "return 1;",
                "return undefined;",
            ),
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let out = run(&plugins, "slow", "{}").await;
    let took = started.elapsed();
    assert!(!out.is_error, "{:?}", out.content);
    assert_eq!(&*out.content, "after 8 s");
    assert!(took >= Duration::from_secs(7), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(notes.is_empty(), "{notes:?}");
}

#[tokio::test]
async fn async_long_command_waits() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow_command",
        call_manifest(),
        &[(
            "index.js",
            &waits(
                "return 1;",
                &format!(r#"{SLEEP} return "after 8 s";"#),
                "return undefined;",
            ),
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let text = plugins.command("slow", "").await;
    let took = started.elapsed();
    assert_eq!(text.as_deref(), Some("after 8 s"));
    assert!(took >= Duration::from_secs(7), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(notes.is_empty(), "{notes:?}");
}

#[tokio::test]
async fn async_long_spin_still_stops() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow_spin",
        call_manifest(),
        &[(
            "index.js",
            &waits(
                r#"await new Promise((done) => setTimeout(done, 100)); while (true) {}"#,
                "return 1;",
                "return undefined;",
            ),
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let out = run(&plugins, "slow", "{}").await;
    let took = started.elapsed();
    assert!(out.is_error, "a stopped tool is an error result");
    assert!(
        took < Duration::from_secs(20),
        "the spin was not stopped early: it took {took:?}"
    );
    assert!(took >= Duration::from_secs(4), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin slow_spin: tool slow stopped after 5 s"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn async_long_hook_keeps_5s() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow_hook",
        call_manifest(),
        &[(
            "index.js",
            &waits(
                r#"return "the tool ran";"#,
                "return 1;",
                r#"await new Promise((done) => setTimeout(done, 8000)); return undefined;"#,
            ),
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let verdict = plugins.before("slow", "{}").await;
    let took = started.elapsed();
    assert_eq!(verdict, CallVerdict::Keep, "the call goes on unchanged");
    assert!(took < Duration::from_secs(6), "it took {took:?}");
    assert!(took >= Duration::from_secs(4), "it took {took:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin slow_hook: hook tool.call stopped after 5 s"),
        "{notes:?}"
    );
    assert_eq!(&*run(&plugins, "slow", "{}").await.content, "the tool ran");
}

// The runtimes of the plugins: each plugin has a realm of its own, so a builtin
// it patches, the memory it allocates and the timers it starts stay its own.

#[tokio::test]
async fn runtime_builtin_stays_in_its_plugin() {
    let root = temp_root();
    plugin(
        root.path(),
        "a",
        manifest(),
        &[(
            "index.js",
            r#"
export default function () {
  Array.prototype.map = () => "patched";
  JSON.stringify = () => "patched";
  globalThis.mark = 1;
}
"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "b",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "check",
    description: "Reads the builtins.",
    parameters: { type: "object" },
    run() {
      return `${[1, 2].map((x) => x * 2).join(",")} ${JSON.stringify({ k: 1 })} ${typeof globalThis.mark}`;
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        &*run(&plugins, "check", "{}").await.content,
        r#"2,4 {"k":1} undefined"#
    );
}

#[tokio::test]
async fn runtime_memory_cap_is_per_plugin() {
    let root = temp_root();
    plugin(
        root.path(),
        "big",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "grow",
    description: "Allocates about 100 MiB.",
    parameters: { type: "object" },
    run() {
      const kept = [];
      for (let i = 0; i < 100; i += 1) kept.push("x".repeat(1 << 20) + i);
      return String(kept.length);
    },
  });
  r.registerTool({
    name: "small",
    description: "Answers.",
    parameters: { type: "object" },
    run() {
      return "ok";
    },
  });
}
"#,
        )],
    )
    .await;
    plugin(
        root.path(),
        "small",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.registerTool({
    name: "tiny",
    description: "Answers.",
    parameters: { type: "object" },
    run() {
      return "fine";
    },
  });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let out = run(&plugins, "grow", "{}").await;
    assert!(out.is_error, "a call past the cap fails: {out:?}");
    let notes = plugins.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("plugin big: out of memory")),
        "{notes:?}"
    );
    // The plugin that filled its runtime keeps working, and so does the one
    // beside it.
    assert_eq!(&*run(&plugins, "small", "{}").await.content, "ok");
    assert_eq!(&*run(&plugins, "tiny", "{}").await.content, "fine");
}

#[tokio::test]
async fn runtime_timers_run_per_plugin() {
    let root = temp_root();
    for (folder, tool) in [("a", "tick_a"), ("b", "tick_b")] {
        let index = format!(
            r#"
setTimeout(() => {{ globalThis.mark = "{folder}"; }}, 20);
export default function (r) {{
  r.registerTool({{
    name: "{tool}",
    description: "Reads the timer's mark.",
    parameters: {{ type: "object" }},
    run() {{
      return globalThis.mark ?? "none";
    }},
  }});
}}
"#
        );
        plugin(root.path(), folder, manifest(), &[("index.js", &index)]).await;
    }
    let plugins = loaded(root.path()).await;
    // Every runtime's own pump runs while the other one answers a job.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(&*run(&plugins, "tick_a", "{}").await.content, "a");
    assert_eq!(&*run(&plugins, "tick_b", "{}").await.content, "b");
}

#[tokio::test]
async fn runtime_unload_ends_every_runtime() {
    let root = temp_root();
    for folder in ["a", "b", "c"] {
        plugin(
            root.path(),
            folder,
            manifest(),
            &[(
                "index.js",
                r#"
setInterval(() => { globalThis.ticks = (globalThis.ticks ?? 0) + 1; }, 10);
export default function () {}
"#,
            )],
        )
        .await;
    }
    let plugins = loaded(root.path()).await;
    // The live state the plugin thread holds a copy of: it is free only once
    // every runtime of the thread is gone.
    let live = std::sync::Arc::clone(&plugins.live);
    drop(plugins);
    let started = Instant::now();
    while std::sync::Arc::strong_count(&live) > 1 {
        assert!(
            started.elapsed() < WAIT,
            "the plugin thread still holds its runtimes"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// The gate a `tool.call` layer is: the permission its manifest needs, the
// limit it may name, and what it does when the layer does not answer.

#[tokio::test]
async fn call_gate_a_hook_without_the_hooks_permission_throws() {
    let root = temp_root();
    plugin(
        root.path(),
        "guard",
        manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => ({ block: "no" }));
}
"#,
        )],
    )
    .await;
    let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
    assert!(plugins.is_none(), "the layer was accepted");
    assert!(
        notes
            .iter()
            .any(|note| note == "plugin guard: plugin guard has no hooks permission for tool.call"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn call_gate_block_on_timeout_blocks_a_silent_handler() {
    let root = temp_root();
    plugin(
        root.path(),
        "silent",
        call_manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => new Promise(() => {}), { timeoutMs: 200, onTimeout: "block" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let verdict = plugins.before("shout", "{}").await;
    let took = started.elapsed();
    assert_eq!(
        verdict,
        CallVerdict::Block("plugin silent: tool.call hook timed out after 200 ms".into())
    );
    assert!(took < Duration::from_secs(1), "it took {took:?}");
}

/// A gate may wait on its coordinator longer than one slice of JS may run: its
/// own `timeoutMs` is the wait, as a tool's promise wait is.
#[tokio::test]
async fn call_gate_a_slow_answer_inside_its_limit_counts() {
    let root = temp_root();
    plugin(
        root.path(),
        "slow",
        call_manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook(
    "tool.call",
    () => new Promise((resolve) => setTimeout(() => resolve({ block: "late" }), 6000)),
    { timeoutMs: 10000, onTimeout: "keep" },
  );
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.before("shout", "{}").await,
        CallVerdict::Block("late".into())
    );
}

#[tokio::test]
async fn call_gate_keep_on_timeout_keeps_a_silent_handler() {
    let root = temp_root();
    plugin(
        root.path(),
        "silent",
        call_manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => new Promise(() => {}), { timeoutMs: 200, onTimeout: "keep" });
  r.registerTool({ name: "shout", description: "shouts", run: () => "shouted" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    let started = Instant::now();
    let verdict = plugins.before("shout", "{}").await;
    let took = started.elapsed();
    assert_eq!(verdict, CallVerdict::Keep, "the call goes on unchanged");
    assert!(took < Duration::from_secs(1), "it took {took:?}");
    assert_eq!(&*run(&plugins, "shout", "{}").await.content, "shouted");
}

#[tokio::test]
async fn call_gate_a_throw_blocks_with_its_message() {
    let root = temp_root();
    plugin(
        root.path(),
        "thrower",
        call_manifest(),
        &[(
            "index.js",
            r#"
export default function (r) {
  r.hook("tool.call", () => { throw new Error("boom"); }, { onTimeout: "block" });
}
"#,
        )],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(
        plugins.before("shout", "{}").await,
        CallVerdict::Block("plugin thrower: tool.call hook failed: boom".into())
    );
}

#[tokio::test]
async fn call_gate_bad_options_throw() {
    for options in [
        "{ timeoutMs: 0 }",
        "{ timeoutMs: 121000 }",
        "{ timeoutMs: 1.5 }",
        "{ timeoutMs: 'soon' }",
        "{ onTimeout: 'wait' }",
        "{ onTimeout: 7 }",
        "{ timeout: 200 }",
        "{ onTimeOut: 'block' }",
        "{ timeoutMs: -5 }",
    ] {
        let root = temp_root();
        plugin(
            root.path(),
            "picky",
            call_manifest(),
            &[(
                "index.js",
                &format!(
                    "export default function (r) {{ r.hook(\"tool.call\", () => undefined, {options}); }}\n"
                ),
            )],
        )
        .await;
        let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
        assert!(plugins.is_none(), "{options} was accepted: {notes:?}");
        assert!(
            notes
                .iter()
                .any(|note| note.contains("timeoutMs") || note.contains("onTimeout")),
            "{options}: {notes:?}"
        );
        if options.contains("-5") {
            assert!(
                notes.iter().any(|note| note.contains("not -5")),
                "the range error names the value: {notes:?}"
            );
        }
    }
}

// The sessions a plugin may start: the permission its manifest names, the
// object that appears with it, and the asks it hands the run.

/// The `package.json` of a plugin that may start sessions.
fn session_manifest() -> &'static str {
    r#"{"name": "__NAME__", "version": "1.0.0", "type": "module", "reagent": {"entry": "index.js", "permissions": {"sessions": true}}}"#
}

/// The plugin program that answers what `typeof reagent.sessions` is.
fn session_typeof() -> &'static str {
    r#"
export default function (r) {
  r.registerTool({
    name: "probe",
    description: "says what reagent.sessions is",
    run: () => typeof r.sessions,
  });
}
"#
}

#[tokio::test]
async fn plugin_session_permission_true_shows_the_object() {
    let root = temp_root();
    plugin(
        root.path(),
        "talker",
        session_manifest(),
        &[("index.js", session_typeof())],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(&*run(&plugins, "probe", "{}").await.content, "object");
}

#[tokio::test]
async fn plugin_session_permission_absent_hides_the_object() {
    let root = temp_root();
    plugin(
        root.path(),
        "quiet",
        manifest(),
        &[("index.js", session_typeof())],
    )
    .await;
    let plugins = loaded(root.path()).await;
    assert_eq!(&*run(&plugins, "probe", "{}").await.content, "undefined");
}

#[tokio::test]
async fn plugin_session_permission_of_a_wrong_value_names_it() {
    for wrong in ["\"yes\"", "\"true\"", "1", "null", "{}"] {
        let root = temp_root();
        let package = manifest().replace(
            r#""entry": "index.js""#,
            &format!(r#""entry": "index.js", "permissions": {{ "sessions": {wrong} }}"#),
        );
        plugin(
            root.path(),
            "odd",
            &package,
            &[("index.js", session_typeof())],
        )
        .await;
        let (plugins, notes) = Plugins::load(root.path(), &state(), None).await;
        assert!(plugins.is_none(), "{wrong} was accepted");
        assert!(
            notes.iter().any(|note| note.contains("`sessions`")),
            "{wrong}: {notes:?}"
        );
    }
}
