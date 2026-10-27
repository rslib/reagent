//! `codemode`: run JavaScript that calls the registry's tools.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use rquickjs::{
    Array, Context, Ctx, Exception, Function, IntoJs, Persistent, Promise, Runtime, Value,
    function::Opt,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use reagent_agent::batch::run_one;
use reagent_agent::guard::Guard;
use reagent_agent::tool::{
    Effects, Object, Registry, Repaired, Reroute, Tool, ToolOutput, parse_args,
};
use reagent_ai::{Text, ToolCall, ToolSpec};
use serde_json::Value as JsonValue;

/// The tool's name.
pub const NAME: &str = "codemode";

/// The tools a script may not call, beside its own.
const EXPAND: &str = "expand";

/// The lean text before the example and the return lines.
const DESCRIPTION: &str = "Run JavaScript that calls other tools; only what it prints or returns comes back. Use it for many independent calls or to cut large output down; when the next step needs your reasoning on a result, call the tool directly. The code is an async function body in a QuickJS sandbox with no file system, network or timers. `tools.<name>({...})` runs a tool with the same arguments as its direct call and returns its text, or an object where a line below shows one; a failed call throws an Error with the tool's message. `print(...)` adds a line; `return value` adds the value. Calls made before an error are not undone.";

/// The example line, shown when the registry has a `read`.
const EXAMPLE: &str = "Example: const [a, b] = await Promise.all([tools.read({path: \"a.txt\"}), tools.read({path: \"b.txt\"})]); print(a.length, b.length)";

/// The sandbox globals: `tools`, whose call lands in `__call`, and the four
/// console methods and `print`, which lands in `__print`.
const PRELUDE: &str = r#"
globalThis.tools = new Proxy({}, {
  get(target, name) {
    return (args) => __call(String(name), JSON.stringify(args ?? {}));
  }
});
globalThis.print = function (...args) {
  __print(args.map(function (arg) {
    if (arg instanceof Promise) {
      __promise();
      return "[Promise]";
    }
    return typeof arg === "string" ? arg : JSON.stringify(arg);
  }).join(" "));
};
globalThis.console = { log: print, info: print, warn: print, error: print };
"#;

/// The error a script gets when it waits on a promise no tool call settles.
const NEVER: &str =
    "the script can never finish: it awaits a promise that no tool call will settle";

/// The error a nested call gets when the token fires.
const CANCELLED: &str = "cancelled";

/// The most calls a script runs at the same time, all of them read-only.
const PARALLEL_CAP: usize = 8;

/// The longest one script may run, until the CLI passes a shorter time.
const WALL_TIME: Duration = Duration::from_secs(10 * 60);

/// The most bytes a script may allocate.
const MEMORY_CAP: usize = 64 * 1024 * 1024;

/// The error a run reports when the script passed the memory cap.
const MEMORY_FAILED: &str = "memory limit: the script passed 64 MB";

/// The text QuickJS puts in the error it raises when an allocation fails.
const OUT_OF_MEMORY: &str = "out of memory";

/// The most tool calls a script may make.
const CALL_CAP: u32 = 50;

/// The error a script gets when it makes the call past the cap.
const CALL_LIMIT: &str = "call limit: a script may make at most 50 tool calls";

/// The error an engine reports when its replies stop coming with no stop set.
const UNANSWERED: &str = "the script's tool calls went unanswered";

/// The most bytes of output a result holds before it is cut to its ends.
const OUTPUT_CAP: usize = 32 * 1024;

/// The most bytes the head, and the tail, of a cut result keeps.
const OUTPUT_HALF: usize = 16 * 1024;

/// The most printed bytes the engine holds, so a runaway loop stays small.
const PRINT_CAP: usize = 8 * 1024 * 1024;

/// The line a cut result adds after the printed output stopped growing.
const PRINT_STOPPED: &str = "[print stopped at 8 MiB]";

/// The note a result adds when the code came in a markdown fence.
const FENCE_NOTE: &str = "note: the code was in a markdown fence; send the code alone";

/// The note a result adds when a printed value was a promise.
const PROMISE_NOTE: &str = "note: a printed value was a Promise; put await before the tools call";

/// The note a result adds when the script did nothing at all.
const NOTHING_NOTE: &str = "note: the script printed nothing, returned nothing and called no tool";

/// The most bytes of a failed call's reason the call list keeps.
const REASON_CAP: usize = 120;

/// How many taken names a spill skips before it gives up.
const SPILL_TRIES: u32 = 64;

/// The start of a spill file's name.
const SPILL_PREFIX: &str = "codemode";

/// The number in the name of the next spill file.
static NEXT_SPILL: AtomicU64 = AtomicU64::new(0);

/// The prefix of every tool a server exposes.
const MCP_PREFIX: &str = "mcp__";

/// The separator between the server and the tool of an exposed name.
const SERVER_SEPARATOR: &str = "__";

/// The bytes one estimated token stands for.
const TOKEN_BYTES: usize = 4;

/// The most bytes the hidden signature lines take: 3000 estimated tokens.
const SIGNATURE_BUDGET: usize = 3_000 * TOKEN_BYTES;

/// Why a script stopped, held in an [`AtomicU8`] both sides share.
#[derive(Clone, Copy)]
#[repr(u8)]
enum Stop {
    /// Nothing asked the script to stop.
    None = 0,
    /// It ran past its wall time.
    Time = 1,
    /// The caller cancelled it.
    Cancel = 2,
}

impl Stop {
    fn as_byte(self) -> u8 {
        self as u8
    }

    /// Whether `signal` holds this value.
    fn is(self, signal: &AtomicU8) -> bool {
        signal.load(Ordering::Relaxed) == self.as_byte()
    }

    /// Sets `signal` to this value.
    fn set(self, signal: &AtomicU8) {
        signal.store(self.as_byte(), Ordering::Relaxed);
    }
}

/// Stops the engine when the run's future is dropped before it ends, as a
/// caller's own cancel `select!` does; after the engine ended it changes nothing.
struct StopOnDrop(Arc<AtomicU8>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = self.0.compare_exchange(
            Stop::None.as_byte(),
            Stop::Cancel.as_byte(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// Why the run must end, when `signal` says it must.
fn stopped(signal: &AtomicU8, wall_time: Duration) -> Option<String> {
    if Stop::Time.is(signal) {
        Some(time_limit(wall_time))
    } else if Stop::Cancel.is(signal) {
        Some(CANCELLED.into())
    } else {
        None
    }
}

/// The error a run past its wall time reports.
fn time_limit(wall_time: Duration) -> String {
    if wall_time < Duration::from_secs(1) {
        format!(
            "time limit: the script ran past {} ms",
            wall_time.as_millis()
        )
    } else {
        format!("time limit: the script ran past {} s", wall_time.as_secs())
    }
}

/// `message` with the memory cap's own error in place of QuickJS's own text.
fn failed(message: String) -> String {
    if message.contains(OUT_OF_MEMORY) {
        MEMORY_FAILED.into()
    } else {
        message
    }
}

/// Where a script gets a tool its own registry does not hold.
pub trait ToolSource: Send + Sync {
    /// The tool `name` means, when it is this source's; `None` when it is no
    /// tool at all.
    fn resolve<'a>(&'a self, name: &'a str)
    -> BoxFuture<'a, Option<Result<Box<dyn Tool>, String>>>;

    /// The names this source can answer for, for `listTools`.
    fn names(&self) -> Vec<String>;

    /// Brings the names and tools this source answers for up to date; the
    /// default, a source that never changes, costs nothing.
    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Counts the changes to the tool list, so a script keeps what it built
    /// while the count holds; the default, for a source that does not track
    /// its list, is [`UNTRACKED`], and its data is built on every script.
    fn generation(&self) -> u64 {
        UNTRACKED
    }
}

/// The generation of a source that does not track its tool list.
const UNTRACKED: u64 = 0;

/// Runs JavaScript that calls other tools, and returns what it printed.
pub struct Codemode {
    spec: ToolSpec,
    registry: Arc<Registry>,
    tools: String,
    /// The registry names the model does not see directly.
    hidden: HashSet<String>,
    source: Option<Arc<dyn ToolSource>>,
    /// The data `describeTool` and `listTools` answer from, and the names it
    /// holds, kept for one generation of the source between scripts.
    listed: Mutex<Option<Listed>>,
    /// The source's generation when the registry's server tools were taken,
    /// so a later one says their specs may be old.
    start_generation: u64,
    guard: Option<Guard>,
    wall_time: Duration,
}

/// One generation of the tools `describeTool` and `listTools` answer for.
#[derive(Clone)]
struct Listed {
    /// The generation of the source it was built at.
    generation: u64,
    /// The tools, in the order the script sees them.
    described: Arc<[Described]>,
    /// The names in `described`, so one call looks a name up once.
    names: Arc<HashSet<String>>,
}

impl std::fmt::Debug for Codemode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Codemode")
            .field("tools", &self.tools)
            .field("hidden", &self.hidden)
            .field("has_source", &self.source.is_some())
            .finish_non_exhaustive()
    }
}

impl Codemode {
    /// Builds the tool over `registry`, which holds the tools a script may call.
    pub fn new(registry: Arc<Registry>) -> Self {
        let (description, tools) = describe(&registry, &HashSet::new());
        Self {
            spec: ToolSpec {
                name: NAME.into(),
                description,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "code": { "type": "string", "description": "JavaScript source." }
                    },
                    "required": ["code"]
                }),
            },
            registry,
            tools,
            hidden: HashSet::new(),
            source: None,
            listed: Mutex::new(None),
            start_generation: UNTRACKED,
            guard: None,
            wall_time: WALL_TIME,
        }
    }

    /// Marks the registry tools `names` as ones the model does not see, so the
    /// description names them only by signature.
    #[must_use]
    pub fn with_hidden(mut self, names: Vec<String>) -> Self {
        self.hidden = names.into_iter().collect();
        self.rebuild();
        self
    }

    /// Resolves the tools a script calls that the registry does not hold, such
    /// as those of a server that is still starting.
    #[must_use]
    pub fn with_source(mut self, source: Arc<dyn ToolSource>) -> Self {
        self.start_generation = source.generation();
        self.source = Some(source);
        self
    }

    /// Whether the source's list changed since the registry took its server
    /// tools, so a hidden tool's spec and adapter come from the source.
    fn moved(&self, generation: u64) -> bool {
        generation != UNTRACKED && generation != self.start_generation
    }

    /// Checks each nested call against `guard`, which refuses a call that runs.
    #[must_use]
    pub fn with_guard(mut self, guard: Guard) -> Self {
        self.guard = Some(guard);
        self
    }

    /// Stops a script that runs past `wall_time`, the shorter the better.
    #[must_use]
    pub fn with_wall_time(mut self, wall_time: Duration) -> Self {
        self.wall_time = wall_time;
        self
    }

    /// Rewrites the description after the hidden list changed.
    fn rebuild(&mut self) {
        let (description, tools) = describe(&self.registry, &self.hidden);
        self.spec.description = description;
        self.tools = tools;
    }

    /// What `describeTool` and `listTools` answer from, taken before a script
    /// starts so both are plain data inside it, and rebuilt only when the
    /// source's list changed since the data was built.
    async fn listing(&self) -> Listed {
        if let Some(source) = &self.source {
            source.refresh().await;
        }
        let generation = self
            .source
            .as_ref()
            .map_or(UNTRACKED, |source| source.generation());
        if let Some(listed) = self.built(generation) {
            return listed;
        }
        let described: Arc<[Described]> = self.described(self.moved(generation)).await.into();
        let names = Arc::new(described.iter().map(|entry| entry.name.clone()).collect());
        let listed = Listed {
            generation,
            described,
            names,
        };
        if generation != UNTRACKED {
            *self.listed_mutex() = Some(listed.clone());
        }
        listed
    }

    /// The data built at `generation`, when that is still the source's.
    fn built(&self, generation: u64) -> Option<Listed> {
        if generation == UNTRACKED {
            return None;
        }
        let listed = self.listed_mutex();
        let listed = listed.as_ref()?;
        (listed.generation == generation).then(|| listed.clone())
    }

    fn listed_mutex(&self) -> MutexGuard<'_, Option<Listed>> {
        self.listed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// What `describeTool` and `listTools` answer from, taken before a script
    /// starts so both are plain data inside it.
    async fn described(&self, moved: bool) -> Vec<Described> {
        let names = self.source.as_ref().map(|source| source.names());
        let known: HashSet<&str> = names.iter().flatten().map(String::as_str).collect();
        let mut described: Vec<Described> = self
            .registry
            .specs()
            .iter()
            .filter(|spec| callable(&spec.name))
            .filter(|spec| match &names {
                None => true,
                Some(_) => {
                    !self.hidden.contains(&spec.name)
                        || (!moved && known.contains(spec.name.as_str()))
                }
            })
            .map(|spec| Described {
                name: spec.name.clone(),
                description: spec.description.clone(),
                input_schema: spec.input_schema.clone(),
                output_schema: self
                    .registry
                    .get(&spec.name)
                    .and_then(|tool| tool.output_schema())
                    .cloned(),
            })
            .collect();
        let Some(source) = &self.source else {
            return described;
        };
        let names = names.unwrap_or_default();
        let mut extra = Vec::new();
        {
            let mut seen: HashSet<&str> =
                described.iter().map(|entry| entry.name.as_str()).collect();
            for name in &names {
                if !seen.insert(name.as_str()) {
                    continue;
                }
                if let Some(Ok(tool)) = source.resolve(name).await {
                    let spec = tool.spec();
                    extra.push(Described {
                        name: name.clone(),
                        description: spec.description.clone(),
                        input_schema: spec.input_schema.clone(),
                        output_schema: tool.output_schema().cloned(),
                    });
                }
            }
        }
        described.extend(extra);
        described
    }
}

/// One tool as `describeTool` writes it.
struct Described {
    name: String,
    description: String,
    input_schema: JsonValue,
    output_schema: Option<JsonValue>,
}

/// The description of a script tool over `registry`, with the names of its
/// tools that the model does not see directly, and the callable names.
fn describe(registry: &Registry, hidden: &HashSet<String>) -> (String, String) {
    let mut tools = String::new();
    let mut shapes = String::new();
    let mut has_read = false;
    for spec in registry.specs() {
        if !callable(&spec.name) {
            continue;
        }
        if !tools.is_empty() {
            tools.push_str(", ");
        }
        tools.push_str(&spec.name);
        has_read |= spec.name == "read";
        // A hidden tool's shape is in its signature line, which comes below.
        if hidden.contains(&spec.name) {
            continue;
        }
        let shape = registry
            .get(&spec.name)
            .map_or("string", |tool| tool.returns());
        if shape != "string" {
            let _ = writeln!(shapes, "`tools.{}` returns {shape}", spec.name);
        }
    }
    let mut body = String::new();
    if has_read {
        body.push_str(EXAMPLE);
        body.push('\n');
    }
    body.push_str(&shapes);
    push_signatures(&mut body, registry, hidden);
    let mut description = String::with_capacity(DESCRIPTION.len() + body.len() + 256);
    description.push_str(DESCRIPTION);
    if !body.is_empty() {
        description.push_str("\n\n");
        description.push_str(&body);
    }
    (description, tools)
}

/// Adds one signature line per hidden tool while they fit in
/// [`SIGNATURE_BUDGET`], then one line per server with the rest.
fn push_signatures(body: &mut String, registry: &Registry, hidden: &HashSet<String>) {
    if hidden.is_empty() {
        return;
    }
    let mut used = 0;
    let mut rest: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut past = false;
    for spec in registry.specs() {
        if !hidden.contains(&spec.name) {
            continue;
        }
        if !past {
            let shape = registry
                .get(&spec.name)
                .map_or("string", |tool| tool.returns());
            let line = format!(
                "`tools.{}({{{}}})` -> {shape}",
                spec.name,
                signature(&spec.input_schema)
            );
            if used + line.len() < SIGNATURE_BUDGET {
                used += line.len() + 1;
                body.push_str(&line);
                body.push('\n');
                continue;
            }
            past = true;
        }
        *rest.entry(server_prefix(&spec.name)).or_default() += 1;
    }
    for (prefix, count) in rest {
        let _ = writeln!(
            body,
            "`{prefix}`: {count} more tools; call listTools(\"{prefix}\") and describeTool(name) in a script"
        );
    }
}

/// The arguments of a signature line: the required names first, then the
/// others sorted, each with a `?` unless the schema requires it.
fn signature(schema: &JsonValue) -> String {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(JsonValue::as_array)
        .map(|names| names.iter().filter_map(JsonValue::as_str).collect())
        .unwrap_or_default();
    let mut parts: Vec<String> = required.iter().map(|name| (*name).to_owned()).collect();
    if let Some(properties) = schema.get("properties").and_then(JsonValue::as_object) {
        let mut others: Vec<String> = properties
            .keys()
            .filter(|name| !required.contains(&name.as_str()))
            .map(|name| format!("{name}?"))
            .collect();
        others.sort();
        parts.extend(others);
    }
    parts.join(", ")
}

/// The `mcp__<server>` part of an exposed name.
fn server_prefix(name: &str) -> &str {
    let Some(rest) = name.strip_prefix(MCP_PREFIX) else {
        return name;
    };
    match rest.find(SERVER_SEPARATOR) {
        Some(at) => &name[..MCP_PREFIX.len() + at],
        None => name,
    }
}

/// A boxed tool, so a tool resolved after the script started can join a
/// registry of its own.
struct Boxed(Box<dyn Tool>);

impl Tool for Boxed {
    fn spec(&self) -> &ToolSpec {
        self.0.spec()
    }

    fn read_only(&self) -> bool {
        self.0.read_only()
    }

    fn returns(&self) -> &'static str {
        self.0.returns()
    }

    fn main_arg(&self) -> Option<&'static str> {
        self.0.main_arg()
    }

    fn effects(&self, args: &str) -> Effects {
        self.0.effects(args)
    }

    fn repair<'a>(&'a self, args: &'a str) -> Option<BoxFuture<'a, Repaired>> {
        self.0.repair(args)
    }

    fn reroute<'a>(&'a self, args: &'a str) -> Option<BoxFuture<'a, Option<Reroute>>> {
        self.0.reroute(args)
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        self.0.run(args, cancel)
    }
}

impl Tool for Codemode {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn read_only(&self) -> bool {
        false
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            // A cancel that came before the run never starts the engine.
            if cancel.is_cancelled() {
                return ToolOutput::error(CANCELLED);
            }
            let code = match parse_args::<Args>(args) {
                Ok(args) => args.code,
                Err(output) => return output,
            };
            let (code, fenced) = unfence(code);
            let wall_time = self.wall_time;
            let started = Instant::now();
            let deadline = tokio::time::Instant::now() + wall_time;
            let stop = Arc::new(AtomicU8::new(Stop::None.as_byte()));
            let _stop_on_drop = StopOnDrop(Arc::clone(&stop));
            // A refresh waits at most each server's timeout; a cancel ends it.
            let described = tokio::select! {
                listed = self.listing() => listed,
                () = cancel.cancelled() => return ToolOutput::error(CANCELLED),
            };
            let listed_now = Arc::clone(&described.names);
            let moved = self.moved(described.generation);
            let described = Arc::clone(&described.described);
            let (requests, mut asked) = unbounded_channel::<Request>();
            let (replies, answered) = std::sync::mpsc::channel::<Reply>();
            let engine_stop = Arc::clone(&stop);
            let engine = tokio::task::spawn_blocking(move || {
                run_script(code, described, requests, answered, engine_stop, wall_time)
            });
            let mut running = FuturesUnordered::new();
            let mut queued: VecDeque<Pending> = VecDeque::new();
            let mut calls: Vec<(String, CallStatus)> = Vec::new();
            let mut open = true;
            let mut busy = 0usize;
            let mut writing = false;
            loop {
                while let Some(pending) = queued.front() {
                    if pending.read_only {
                        if writing || busy >= PARALLEL_CAP {
                            break;
                        }
                    } else if busy > 0 {
                        break;
                    }
                    let Some(Pending { request, read_only }) = queued.pop_front() else {
                        break;
                    };
                    let Request { id, name, args } = request;
                    let tool = self.registry.resolve(&name);
                    let skipped =
                        !callable(&name) || tool.is_some_and(|tool| !callable(&tool.spec().name));
                    if skipped {
                        let text: Text = unknown(&name, &self.tools);
                        calls.push((name, CallStatus::Error(Arc::clone(&text))));
                        let _ = replies.send(Reply {
                            id,
                            outcome: ReplyOutcome::Error(text),
                        });
                        continue;
                    }
                    // A server's tool may arrive after the script started: the
                    // source waits for it and runs it in a registry of its own,
                    // so the guard, the repairs and the coercion still apply.
                    // A hidden tool goes to the source too once the servers'
                    // lists changed, so it runs with its new spec, or gets the
                    // unknown-tool error when the servers no longer list it.
                    let late = self.source.is_some()
                        && ((tool.is_none() && name.starts_with(MCP_PREFIX))
                            || (tool.is_some()
                                && self.hidden.contains(&name)
                                && (moved || !listed_now.contains(&name))));
                    // A name no tool has goes on to `run_one`, whose error hints at the call meant.
                    let listed = tool.map_or_else(|| name.clone(), |tool| tool.spec().name.clone());
                    let at = calls.len();
                    calls.push((listed, CallStatus::Ok));
                    let call = ToolCall {
                        id: Text::from(id.to_string()),
                        name: Text::from(name),
                        arguments: Text::from(args),
                    };
                    running.push(async move {
                        let output = if late {
                            match &self.source {
                                Some(source) => match source.resolve(&call.name).await {
                                    Some(Ok(tool)) => {
                                        let mut registry = Registry::new();
                                        registry.add(Boxed(tool));
                                        run_one(&registry, self.guard.as_ref(), &call, cancel).await
                                    }
                                    Some(Err(reason)) => Some(ToolOutput::error(reason)),
                                    None => {
                                        Some(ToolOutput::error(unknown(&call.name, &self.tools)))
                                    }
                                },
                                None => Some(ToolOutput::error(unknown(&call.name, &self.tools))),
                            }
                        } else {
                            run_one(&self.registry, self.guard.as_ref(), &call, cancel).await
                        };
                        (id, at, output)
                    });
                    busy += 1;
                    if !read_only {
                        writing = true;
                    }
                }
                if !open && queued.is_empty() && busy == 0 {
                    break;
                }
                tokio::select! {
                    () = cancel.cancelled() => {
                        Stop::Cancel.set(&stop);
                        break;
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        Stop::Time.set(&stop);
                        break;
                    }
                    request = asked.recv(), if open => {
                        match request {
                            Some(request) => {
                                let read_only = self
                                    .registry
                                    .resolve(&request.name)
                                    .is_some_and(Tool::read_only);
                                queued.push_back(Pending { request, read_only });
                            }
                            None => open = false,
                        }
                    }
                    Some((id, at, output)) = running.next(), if busy > 0 => {
                        busy -= 1;
                        if busy == 0 {
                            writing = false;
                        }
                        let (status, outcome) = match output {
                            None => {
                                let cancelled = Text::from(CANCELLED);
                                (
                                    CallStatus::Error(Arc::clone(&cancelled)),
                                    ReplyOutcome::Error(cancelled),
                                )
                            }
                            Some(output) if output.is_error => (
                                CallStatus::Error(Arc::clone(&output.content)),
                                ReplyOutcome::Error(output.content),
                            ),
                            Some(ToolOutput { content, object, .. }) => (
                                CallStatus::Ok,
                                ReplyOutcome::Ok {
                                    text: content,
                                    object,
                                },
                            ),
                        };
                        if let Some((_, ended)) = calls.get_mut(at) {
                            *ended = status;
                        }
                        let _ = replies.send(Reply { id, outcome });
                    }
                }
            }
            // A stopped run keeps its printed lines: cancel the calls, close the
            // reply channel and let the engine end on its own.
            drop(running);
            drop(replies);
            let script = match engine.await {
                Ok(script) => script,
                Err(_) => Script::failed("the script did not finish"),
            };
            let status = match &script.outcome {
                Outcome::Returned(_) => "completed",
                Outcome::Failed(_) => "failed",
            };
            let mut body = script.printed;
            let printed_nothing = body.is_empty();
            body.reserve(64);
            match &script.outcome {
                Outcome::Returned(Some(value)) => {
                    body.push_str("return: ");
                    body.push_str(value);
                    body.push('\n');
                }
                Outcome::Failed(message) => {
                    body.push_str("error: ");
                    body.push_str(message);
                    body.push('\n');
                }
                Outcome::Returned(None) => {}
            }
            if fenced {
                push_note(&mut body, FENCE_NOTE);
            }
            if script.promise {
                push_note(&mut body, PROMISE_NOTE);
            }
            if printed_nothing
                && calls.is_empty()
                && matches!(&script.outcome, Outcome::Returned(None))
            {
                push_note(&mut body, NOTHING_NOTE);
            }
            let mut out = String::with_capacity(body.len() + 64 + calls.len() * 24);
            let _ = writeln!(
                out,
                "codemode {status} in {} ms",
                started.elapsed().as_millis()
            );
            if body.len() > OUTPUT_CAP {
                push_cut(&mut out, &body).await;
            } else {
                out.push_str(&body);
            }
            if !calls.is_empty() {
                out.push_str("calls:\n");
                for (name, status) in &calls {
                    out.push_str("- ");
                    out.push_str(name);
                    out.push(' ');
                    match status {
                        CallStatus::Ok => out.push_str("ok"),
                        CallStatus::Error(text) => {
                            out.push_str("error: ");
                            out.push_str(reason(text));
                        }
                    }
                    out.push('\n');
                }
            }
            if script.outcome.is_error() {
                ToolOutput::error(out)
            } else {
                ToolOutput::ok(out)
            }
        })
    }
}

/// The error a script gets for a call no tool answers.
fn unknown(name: &str, tools: &str) -> Text {
    format!("unknown tool `{name}`; the tools are: {tools}").into()
}

/// Whether a tool of this name is offered to a script.
fn callable(name: &str) -> bool {
    name != NAME && name != EXPAND
}

/// Writes `body` cut to its head and its tail, with the whole text spilled to a
/// file that the cut line names.
async fn push_cut(out: &mut String, body: &str) {
    out.push_str(&body[..boundary_down(body, OUTPUT_HALF)]);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    match spill(body).await {
        Ok(path) => {
            let _ = writeln!(
                out,
                "[output cut: {} bytes; full output in {}]",
                body.len(),
                path.display()
            );
        }
        Err(error) => {
            let _ = writeln!(
                out,
                "[output cut: {} bytes; cannot save the full output: {error}]",
                body.len()
            );
        }
    }
    out.push_str(&body[boundary_up(body, body.len() - OUTPUT_HALF)..]);
}

/// The last character boundary at or below `at`.
fn boundary_down(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The first character boundary at or above `at`.
fn boundary_up(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Writes `body` to a new file in the spill folder and returns its path.
async fn spill(body: &str) -> Result<PathBuf, String> {
    let folder = reagent_tools::spill::folder().await?;
    let mut tries = 0;
    loop {
        let path = folder.join(format!(
            "{SPILL_PREFIX}-{}-{}.log",
            std::process::id(),
            NEXT_SPILL.fetch_add(1, Ordering::Relaxed)
        ));
        let opened = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await;
        match opened {
            Ok(mut file) => {
                file.write_all(body.as_bytes())
                    .await
                    .map_err(|error| error.to_string())?;
                // A tokio file may still hold the last write when it drops.
                file.flush().await.map_err(|error| error.to_string())?;
                return Ok(path);
            }
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists && tries + 1 < SPILL_TRIES =>
            {
                tries += 1;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// The code inside a markdown fence, and whether the code came in one.
fn unfence(mut code: String) -> (String, bool) {
    let lead = code.len() - code.trim_start().len();
    if !code[lead..].starts_with("```") {
        return (code, false);
    }
    let start = code[lead..]
        .find('\n')
        .map_or(code.len(), |at| lead + at + 1);
    let end = code[start..]
        .rfind("```")
        .map_or(code.len(), |at| start + at);
    code.truncate(end);
    code.drain(..start);
    (code, true)
}

/// Adds one note line to a result's body.
fn push_note(body: &mut String, note: &str) {
    body.push_str(note);
    body.push('\n');
}

/// The first line of a failed call's text, cut to 120 bytes.
fn reason(text: &str) -> &str {
    let first = text.split('\n').next().unwrap_or(text);
    &first[..boundary_down(first, REASON_CAP)]
}

/// The arguments of one call. The code is owned, as the engine thread keeps it.
#[derive(serde::Deserialize)]
struct Args {
    code: String,
}

/// One tool call a script asked for.
struct Request {
    id: u32,
    name: String,
    args: String,
}

/// A request waiting for a slot to run in.
struct Pending {
    request: Request,
    read_only: bool,
}

/// The answer to one request.
struct Reply {
    id: u32,
    outcome: ReplyOutcome,
}

/// What a nested call produced.
enum ReplyOutcome {
    /// The text, and the object a script reads in its place when there is one.
    Ok {
        text: Text,
        object: Option<Box<Object>>,
    },
    Error(Text),
}

/// How one nested call ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CallStatus {
    Ok,
    /// It failed with this text.
    Error(Text),
}

/// How a script itself ended.
enum Outcome {
    /// The script returned; `None` when it returned `undefined`.
    Returned(Option<String>),
    /// It stopped with this message.
    Failed(String),
}

impl Outcome {
    fn is_error(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// What one run printed and how it ended.
struct Script {
    printed: String,
    outcome: Outcome,
    /// Whether a printed value was a promise.
    promise: bool,
}

impl Script {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            printed: String::new(),
            outcome: Outcome::Failed(message.into()),
            promise: false,
        }
    }
}

/// The resolve and reject functions of one promise a nested call waits on.
struct Waiting {
    resolve: Persistent<Function<'static>>,
    reject: Persistent<Function<'static>>,
}

/// What the engine holds while a script runs: its output and its pending calls.
#[derive(Default)]
struct State {
    printed: String,
    next_id: u32,
    waiting: HashMap<u32, Waiting>,
    /// The requests sent that have no reply yet.
    outstanding: usize,
    /// The tool calls the script asked for.
    calls: u32,
    /// The limit the script passed, when it passed one.
    over: Option<&'static str>,
    /// Whether a print past the cap was dropped.
    print_stopped: bool,
    /// Whether a print showed a promise.
    promise_printed: bool,
}

/// Runs one script to its end on this thread. `requests` leaves for the tools
/// and `replies` comes back from them.
fn run_script(
    code: String,
    described: Arc<[Described]>,
    requests: UnboundedSender<Request>,
    replies: Receiver<Reply>,
    stop: Arc<AtomicU8>,
    wall_time: Duration,
) -> Script {
    let Ok(runtime) = Runtime::new() else {
        return Script::failed("the JavaScript runtime did not start");
    };
    runtime.set_memory_limit(MEMORY_CAP);
    runtime.set_interrupt_handler(Some(Box::new({
        let stop = Arc::clone(&stop);
        move || !Stop::None.is(&stop)
    })));
    let Ok(context) = Context::full(&runtime) else {
        return Script::failed("the JavaScript context did not start");
    };
    context.with(|ctx| {
        engine(
            &ctx, &code, &described, &requests, &replies, &stop, wall_time,
        )
    })
}

/// The engine loop: run the code, answer each nested call, and read the result.
fn engine<'js>(
    ctx: &Ctx<'js>,
    code: &str,
    described: &Arc<[Described]>,
    requests: &UnboundedSender<Request>,
    replies: &Receiver<Reply>,
    stop: &AtomicU8,
    wall_time: Duration,
) -> Script {
    let state = Rc::new(RefCell::new(State::default()));
    if sandbox(ctx, &state, described, requests).is_err() {
        return Script::failed("the script sandbox did not start");
    }
    let mut source = String::with_capacity(code.len() + 32);
    source.push_str("(async () => {\n");
    source.push_str(code);
    source.push_str("\n})()");
    let promise = match ctx.eval::<Value, _>(source) {
        Ok(value) => match value.into_promise() {
            Some(promise) => promise,
            None => return finish(&state, Outcome::Failed("the code did not start".into())),
        },
        Err(error) => {
            let outcome = match stopped(stop, wall_time) {
                Some(message) => Outcome::Failed(message),
                None => Outcome::Failed(failed(error_text(ctx, error, code))),
            };
            return finish(&state, outcome);
        }
    };
    let outcome = loop {
        while ctx.execute_pending_job() {}
        if let Some(settled) = promise.result::<Value>() {
            break match settled {
                Ok(value) => Outcome::Returned(returned(ctx, value)),
                Err(_) => match stopped(stop, wall_time) {
                    Some(message) => Outcome::Failed(message),
                    None => Outcome::Failed(failed(thrown_error(ctx, ctx.catch(), code))),
                },
            };
        }
        if let Some(message) = stopped(stop, wall_time) {
            break Outcome::Failed(message);
        }
        if state.borrow().outstanding == 0 {
            break Outcome::Failed(NEVER.into());
        }
        match replies.recv() {
            Ok(reply) => settle(ctx, &state, reply),
            Err(_) => break Outcome::Failed(stopped(stop, wall_time).unwrap_or(UNANSWERED.into())),
        }
    };
    finish(&state, outcome)
}

/// Sets `__call`, `__print` and `__promise`, with `describeTool` and
/// `listTools`, on the sandbox's global object.
fn sandbox<'js>(
    ctx: &Ctx<'js>,
    state: &Rc<RefCell<State>>,
    described: &Arc<[Described]>,
    requests: &UnboundedSender<Request>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let call = Function::new(ctx.clone(), {
        let state = Rc::clone(state);
        let requests = requests.clone();
        move |js: Ctx<'js>, name: String, args: String| -> rquickjs::Result<Promise<'js>> {
            {
                let mut state = state.borrow_mut();
                if state.calls >= CALL_CAP {
                    state.over = Some(CALL_LIMIT);
                    return Err(Exception::throw_message(&js, CALL_LIMIT));
                }
                state.calls += 1;
            }
            let (promise, resolve, reject) = js.promise()?;
            let id = {
                let mut state = state.borrow_mut();
                let id = state.next_id;
                state.next_id += 1;
                state.outstanding += 1;
                state.waiting.insert(
                    id,
                    Waiting {
                        resolve: Persistent::save(&js, resolve),
                        reject: Persistent::save(&js, reject),
                    },
                );
                id
            };
            let _ = requests.send(Request { id, name, args });
            Ok(promise)
        }
    })?;
    globals.set("__call", call)?;
    let print = Function::new(ctx.clone(), {
        let state = Rc::clone(state);
        move |line: String| {
            let mut state = state.borrow_mut();
            if state.printed.len() >= PRINT_CAP {
                state.print_stopped = true;
                return;
            }
            state.printed.push_str(&line);
            state.printed.push('\n');
        }
    })?;
    globals.set("__print", print)?;
    let seen = Function::new(ctx.clone(), {
        let state = Rc::clone(state);
        move || {
            state.borrow_mut().promise_printed = true;
        }
    })?;
    globals.set("__promise", seen)?;
    globals.set("describeTool", describe_fn(ctx, described)?)?;
    globals.set("listTools", list_fn(ctx, described)?)?;
    ctx.eval::<(), _>(PRELUDE)?;
    Ok(())
}

/// The `describeTool` host function: what `name` takes and returns, or null.
fn describe_fn<'js>(
    ctx: &Ctx<'js>,
    described: &Arc<[Described]>,
) -> rquickjs::Result<Function<'js>> {
    let described = Arc::clone(described);
    Function::new(ctx.clone(), move |js: Ctx<'js>, name: String| {
        let Some(entry) = described.iter().find(|entry| entry.name == name) else {
            return Value::new_null(js.clone());
        };
        let Ok(object) = rquickjs::Object::new(js.clone()) else {
            return Value::new_undefined(js.clone());
        };
        let _ = object.set("name", entry.name.as_str());
        let _ = object.set("description", entry.description.as_str());
        if let Ok(schema) = json_value(&js, &entry.input_schema) {
            let _ = object.set("inputSchema", schema);
        }
        if let Some(output) = &entry.output_schema
            && let Ok(schema) = json_value(&js, output)
        {
            let _ = object.set("outputSchema", schema);
        }
        object.into_value()
    })
}

/// The `listTools` host function: the names under `prefix`, or all of them.
fn list_fn<'js>(ctx: &Ctx<'js>, described: &Arc<[Described]>) -> rquickjs::Result<Function<'js>> {
    let described = Arc::clone(described);
    Function::new(ctx.clone(), move |prefix: Opt<String>| {
        let prefix = prefix.0.unwrap_or_default();
        let names: Vec<String> = described
            .iter()
            .filter(|entry| entry.name.starts_with(&prefix))
            .map(|entry| entry.name.clone())
            .collect();
        names
    })
}

/// Resolves or rejects the promise one nested call waited on.
fn settle<'js>(ctx: &Ctx<'js>, state: &RefCell<State>, reply: Reply) {
    let waiting = {
        let mut state = state.borrow_mut();
        let Some(waiting) = state.waiting.remove(&reply.id) else {
            return;
        };
        state.outstanding = state.outstanding.saturating_sub(1);
        waiting
    };
    match reply.outcome {
        ReplyOutcome::Ok { text, object } => {
            let Ok(resolve) = waiting.resolve.restore(ctx) else {
                return;
            };
            let value = object
                .as_deref()
                .and_then(|object| object_value(ctx, &text, object).ok());
            match value {
                Some(value) => {
                    let _ = resolve.call::<_, ()>((value,));
                }
                None => {
                    let _ = resolve.call::<_, ()>((text.as_ref(),));
                }
            }
        }
        ReplyOutcome::Error(message) => {
            if let Ok(reject) = waiting.reject.restore(ctx) {
                let Ok(error) = Exception::from_message(ctx.clone(), message.as_ref()) else {
                    return;
                };
                let _ = reject.call::<_, ()>((error,));
            }
        }
    }
}

/// The JS object a result's object becomes: its fields, and `output` as the
/// text without its last `tail` bytes, or as its numbered lines alone.
fn object_value<'js>(ctx: &Ctx<'js>, text: &str, object: &Object) -> rquickjs::Result<Value<'js>> {
    let value = rquickjs::Object::new(ctx.clone())?;
    for (name, field) in &object.fields {
        value.set(name.as_str(), json_value(ctx, field)?)?;
    }
    if let Some(tail) = object.tail {
        let end = text.len().saturating_sub(tail);
        let output = text
            .split_at_checked(end)
            .map_or(text, |(output, _)| output);
        value.set("output", output)?;
    }
    if object.numbered {
        value.set("output", plain_lines(text))?;
    }
    Ok(value.into_value())
}

/// The numbered lines of `text` without their `{number}\t` prefix, so a script
/// reads the file's own lines and not the notes around them.
fn plain_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split('\n') {
        let Some((number, rest)) = line.split_once('\t') else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        out.push_str(rest);
        out.push('\n');
    }
    out
}

/// A JSON value as the JS value of the same shape.
fn json_value<'js>(ctx: &Ctx<'js>, value: &JsonValue) -> rquickjs::Result<Value<'js>> {
    Ok(match value {
        JsonValue::Null => Value::new_null(ctx.clone()),
        JsonValue::Bool(flag) => Value::new_bool(ctx.clone(), *flag),
        JsonValue::Number(number) => match number.as_i64() {
            Some(whole) => whole.into_js(ctx)?,
            None => number.as_f64().unwrap_or_default().into_js(ctx)?,
        },
        JsonValue::String(text) => text.as_str().into_js(ctx)?,
        JsonValue::Array(items) => {
            let array = Array::new(ctx.clone())?;
            for (at, item) in items.iter().enumerate() {
                array.set(at, json_value(ctx, item)?)?;
            }
            array.into_value()
        }
        JsonValue::Object(fields) => {
            let object = rquickjs::Object::new(ctx.clone())?;
            for (name, field) in fields {
                object.set(name.as_str(), json_value(ctx, field)?)?;
            }
            object.into_value()
        }
    })
}

/// Takes the output so far and lets go of the pending promises.
fn finish(state: &RefCell<State>, outcome: Outcome) -> Script {
    let mut state = state.borrow_mut();
    state.waiting.clear();
    if state.print_stopped {
        state.printed.push_str(PRINT_STOPPED);
        state.printed.push('\n');
    }
    // A script that passed a limit failed, however it caught the error.
    let outcome = match state.over {
        Some(message) => Outcome::Failed(message.into()),
        None => outcome,
    };
    Script {
        printed: std::mem::take(&mut state.printed),
        outcome,
        promise: state.promise_printed,
    }
}

/// The value a script returned, as the result text shows it.
fn returned<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Option<String> {
    if value.is_undefined() {
        return None;
    }
    if let Some(text) = value.as_string() {
        return text.to_string().ok();
    }
    json(ctx, value)
}

/// A value as `JSON.stringify` writes it.
fn json<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Option<String> {
    match ctx.json_stringify(value) {
        Ok(Some(text)) => text.to_string().ok(),
        _ => None,
    }
}

/// The text a failed script shows for `value`, with the name of an exception
/// and the line of `code` that it names.
fn thrown_error<'js>(ctx: &Ctx<'js>, value: Value<'js>, code: &str) -> String {
    let parts = value.as_exception().map(|exception| {
        let name = exception
            .as_object()
            .get::<_, Option<String>>("name")
            .ok()
            .flatten();
        let line = exception_line(exception).map(|line| model_line(line, code));
        (name, line, exception.message().unwrap_or_default())
    });
    let Some((name, line, message)) = parts else {
        return thrown_text(ctx, value);
    };
    let mut error = String::with_capacity(message.len() + 32);
    if let Some(name) = name {
        error.push_str(&name);
        if let Some(line) = line {
            let _ = write!(error, " at line {line}");
        }
        error.push_str(": ");
    }
    error.push_str(&message);
    error
}

/// The line of the first frame of `exception`'s stack.
fn exception_line(exception: &Exception<'_>) -> Option<u32> {
    let stack = exception.stack()?;
    let frame = stack.lines().next()?;
    let place = frame.rsplit_once('(').map_or(frame, |(_, place)| place);
    let mut parts = place.trim_end_matches(')').rsplit(':');
    let _column = parts.next()?;
    parts.next()?.trim().parse().ok()
}

/// The line of the model's own code that a line of the wrapped source names.
fn model_line(line: u32, code: &str) -> u32 {
    let lines = code.lines().count().max(1) as u32;
    line.saturating_sub(1).clamp(1, lines)
}

/// The message of a value a script threw that is no exception.
fn thrown_text<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> String {
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return text;
    }
    json(ctx, value).unwrap_or_default()
}

/// The message of a failure from `eval`, which leaves the reason in the context.
fn error_text<'js>(ctx: &Ctx<'js>, error: rquickjs::Error, code: &str) -> String {
    match error {
        rquickjs::Error::Exception => thrown_error(ctx, ctx.catch(), code),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use futures_util::future::BoxFuture;
    use reagent_ai::ToolSpec;
    use reagent_tools::grep::Grep;
    use reagent_tools::read::Read;
    use serde_json::{Value, json};

    use super::*;

    /// The lean text, word for word.
    const AGREED: &str = "Run JavaScript that calls other tools; only what it prints or returns comes back. Use it for many independent calls or to cut large output down; when the next step needs your reasoning on a result, call the tool directly. The code is an async function body in a QuickJS sandbox with no file system, network or timers. `tools.<name>({...})` runs a tool with the same arguments as its direct call and returns its text, or an object where a line below shows one; a failed call throws an Error with the tool's message. `print(...)` adds a line; `return value` adds the value. Calls made before an error are not undone.";

    /// The example line, word for word.
    const EXAMPLE_LINE: &str = "Example: const [a, b] = await Promise.all([tools.read({path: \"a.txt\"}), tools.read({path: \"b.txt\"})]); print(a.length, b.length)";

    /// A tool whose spec, shape and reply the test sets.
    struct Fake {
        spec: ToolSpec,
        returns: &'static str,
        output_schema: Option<Value>,
        reply: Box<dyn Fn() -> ToolOutput + Send + Sync>,
    }

    impl Fake {
        /// Makes the tool's script shape `returns`.
        fn shape(mut self, returns: &'static str) -> Self {
            self.returns = returns;
            self
        }

        /// Gives the tool an output schema.
        fn output_schema(mut self, schema: Value) -> Self {
            self.output_schema = Some(schema);
            self
        }
    }

    impl Tool for Fake {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn returns(&self) -> &'static str {
            self.returns
        }

        fn output_schema(&self) -> Option<&Value> {
            self.output_schema.as_ref()
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move { (self.reply)() })
        }
    }

    fn fake_replying(
        name: &str,
        schema: Value,
        reply: impl Fn() -> ToolOutput + Send + Sync + 'static,
    ) -> Fake {
        Fake {
            spec: ToolSpec {
                name: name.into(),
                description: "A test tool.".into(),
                input_schema: schema,
            },
            returns: "string",
            output_schema: None,
            reply: Box::new(reply),
        }
    }

    fn fake(name: &str, schema: Value) -> Fake {
        fake_replying(name, schema, || ToolOutput::ok(""))
    }

    /// A tool whose reply sees the arguments the batch sent it.
    struct Reading {
        spec: ToolSpec,
        reply: Box<dyn Fn(&str) -> ToolOutput + Send + Sync>,
    }

    impl Tool for Reading {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move { (self.reply)(args) })
        }
    }

    fn fake_reading(
        name: &str,
        schema: Value,
        reply: impl Fn(&str) -> ToolOutput + Send + Sync + 'static,
    ) -> Reading {
        Reading {
            spec: ToolSpec {
                name: name.into(),
                description: "A test tool.".into(),
                input_schema: schema,
            },
            reply: Box::new(reply),
        }
    }

    /// How many calls of one tool ran at once, and when each one ran.
    #[derive(Default)]
    struct Track {
        running: AtomicUsize,
        peak: AtomicUsize,
        spans: Mutex<Vec<(Instant, Instant)>>,
    }

    impl Track {
        /// Notes a call that starts now and returns when it started.
        fn enter(&self) -> Instant {
            let started = Instant::now();
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(running, Ordering::SeqCst);
            started
        }

        /// Notes the end of the call that started at `started`.
        fn leave(&self, started: Instant) {
            self.running.fetch_sub(1, Ordering::SeqCst);
            self.spans
                .lock()
                .expect("track lock")
                .push((started, Instant::now()));
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        fn spans(&self) -> Vec<(Instant, Instant)> {
            self.spans.lock().expect("track lock").clone()
        }
    }

    /// A tool that sleeps for `sleep` and records its calls in `track`.
    struct Tracked {
        spec: ToolSpec,
        read_only: bool,
        sleep: Duration,
        track: Arc<Track>,
    }

    impl Tool for Tracked {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            self.read_only
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move {
                let started = self.track.enter();
                tokio::time::sleep(self.sleep).await;
                self.track.leave(started);
                ToolOutput::ok("ok")
            })
        }
    }

    fn tracked(name: &str, read_only: bool, sleep: Duration, track: &Arc<Track>) -> Tracked {
        Tracked {
            spec: ToolSpec {
                name: name.into(),
                description: "A timed test tool.".into(),
                input_schema: object(),
            },
            read_only,
            sleep,
            track: Arc::clone(track),
        }
    }

    /// A guard whose rules allow no call.
    fn deny_all_guard() -> reagent_agent::guard::Guard {
        use reagent_config::permissions::{Lists, Rules};
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c/config.toml"),
            settings: reagent_config::Settings {
                permissions: Lists {
                    default: Some("deny".into()),
                    ..Lists::default()
                },
                ..reagent_config::Settings::default()
            },
        };
        let rules = Rules::from_layers(&[layer], std::path::Path::new("/work"), None).unwrap();
        reagent_agent::guard::Guard::new(rules)
    }

    fn object() -> Value {
        json!({"type": "object"})
    }

    fn with_path() -> Value {
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    }

    async fn run_code(codemode: &Codemode, code: &str) -> ToolOutput {
        let args = json!({ "code": code }).to_string();
        codemode.run(&args, &CancellationToken::new()).await
    }

    #[test]
    fn lean_text_is_the_agreed_text() {
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add(Read::new(tmp.path()));
        registry.add(Grep::new(tmp.path()));
        registry.add(reagent_tools::bash::Bash::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let description = codemode.spec().description.as_str();
        assert_eq!(codemode.spec().name, "codemode");
        assert!(
            description.starts_with(&format!("{AGREED}\n\n")),
            "{description}"
        );
        assert!(
            description.lines().any(|line| line == EXAMPLE_LINE),
            "{description}"
        );
        assert!(
            description.lines().any(|line| line
                == "`tools.bash` returns {output, exit_code, truncated, full_output_path?}"),
            "{description}"
        );
        assert!(
            !description
                .lines()
                .any(|line| line.starts_with("read(") || line.starts_with("grep(")),
            "{description}"
        );
    }

    #[test]
    fn lean_text_has_no_example_without_read() {
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add(Grep::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let description = codemode.spec().description.as_str();
        assert!(!description.contains("Example:"), "{description}");
        assert!(
            !description.lines().any(|line| line.starts_with("`tools.")),
            "{description}"
        );
        assert_eq!(description, AGREED);
    }

    #[test]
    fn lean_text_lists_only_object_returns() {
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add(Read::new(tmp.path()));
        registry.add(Grep::new(tmp.path()));
        registry.add(reagent_tools::bash::Bash::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let description = codemode.spec().description.as_str();
        let shapes: Vec<&str> = description
            .lines()
            .filter(|line| line.starts_with("`tools."))
            .collect();
        assert_eq!(
            shapes,
            [
                "`tools.bash` returns {output, exit_code, truncated, full_output_path?}",
                "`tools.read` returns {output, start, end, next_offset?}"
            ],
            "{description}"
        );
        assert!(!description.contains("`tools.grep`"), "{description}");
    }

    #[test]
    fn lean_text_skips_codemode_and_expand() {
        let mut registry = Registry::new();
        registry.add(fake("expand", object()).shape("{code}"));
        registry.add(fake("read", with_path()));
        let codemode = Codemode::new(Arc::new(registry));
        let description = codemode.spec().description.as_str();
        assert!(!description.contains("`tools.expand`"), "{description}");
        assert!(!description.contains("expand("), "{description}");
        assert!(
            !description.lines().any(|line| line.starts_with("`tools.")),
            "{description}"
        );
    }

    #[test]
    fn lean_text_is_small() {
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add(reagent_tools::bash::Bash::new(tmp.path()));
        registry.add(reagent_tools::edit::Edit::new(tmp.path()));
        registry.add(reagent_tools::find::Find::new(tmp.path()));
        registry.add(Grep::new(tmp.path()));
        registry.add(Read::new(tmp.path()));
        registry.add(reagent_tools::write::Write::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let size = serde_json::to_string(codemode.spec()).unwrap().len();
        println!("the codemode spec is {size} bytes");
        assert!(size < 1100, "{size} bytes");
    }

    #[test]
    fn spec_code_is_the_one_required_string() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let schema = &codemode.spec().input_schema;
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert_eq!(properties.len(), 1, "{properties:?}");
        let code = properties.get("code").expect("code");
        assert_eq!(code.get("type").and_then(Value::as_str), Some("string"));
        assert_eq!(
            code.get("description").and_then(Value::as_str),
            Some("JavaScript source.")
        );
        assert_eq!(schema.get("required"), Some(&json!(["code"])));
        assert!(!codemode.read_only());
    }

    #[tokio::test]
    async fn run_two_calls_in_one_script() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\nneedle here\n")
            .await
            .unwrap();
        let mut registry = Registry::new();
        registry.add(Read::new(tmp.path()));
        registry.add(Grep::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "const text = await tools.read({path: \"a.txt\"});\n\
             const hits = await tools.grep({pattern: \"needle\"});\n\
             print(hits.split(\"\\n\").filter((line) => line.includes(\"needle\")).join(\"\\n\"));\n",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.starts_with("codemode completed in"), "{text}");
        assert!(text.contains("a.txt:3:needle here"), "{text}");
        assert!(text.contains("calls:\n- read ok\n- grep ok"), "{text}");
        assert!(!text.contains("alpha"), "{text}");
        assert!(!text.contains("beta"), "{text}");
    }

    #[tokio::test]
    async fn run_failed_call_is_caught() {
        let mut registry = Registry::new();
        registry.add(fake_replying("fail", object(), || {
            ToolOutput::error("no such file")
        }));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "try { await tools.fail({}) } catch (e) { print(e.message) }",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("no such file"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("- fail error"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn run_syntax_error_runs_no_tool() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let mut registry = Registry::new();
        registry.add(fake_replying("count", object(), move || {
            counted.fetch_add(1, Ordering::SeqCst);
            ToolOutput::ok("2")
        }));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(&codemode, "await tools.count({").await;
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("SyntaxError"), "{}", output.content);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn run_failed_script_keeps_output() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "print(\"before\"); throw new Error(\"boom\")").await;
        assert!(output.is_error, "{}", output.content);
        let lines: Vec<&str> = output.content.lines().collect();
        assert!(
            lines
                .first()
                .is_some_and(|line| line.starts_with("codemode failed in")),
            "{lines:?}"
        );
        assert_eq!(lines.get(1), Some(&"before"));
        assert_eq!(lines.get(2), Some(&"error: Error at line 1: boom"));
    }

    #[tokio::test]
    async fn failure_syntax_error_names_kind_and_line() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "print(1)\nprint(").await;
        assert!(output.is_error, "{}", output.content);
        let line = output
            .content
            .lines()
            .find(|line| line.starts_with("error: "))
            .expect("an error line");
        assert!(line.starts_with("error: SyntaxError at line 2:"), "{line}");
    }

    #[tokio::test]
    async fn failure_type_error_names_kind_and_line() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "print(1)\nconst x = undefined\nx.foo()").await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .lines()
                .any(|line| line.starts_with("error: TypeError at line 3:")),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn failure_thrown_error_names_kind() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "throw new Error(\"boom\")").await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .lines()
                .any(|line| line == "error: Error at line 1: boom"),
            "{}",
            output.content
        );
    }

    /// The `error:` line of a failed run.
    fn error_line(output: &ToolOutput) -> &str {
        output
            .content
            .lines()
            .find(|line| line.starts_with("error: "))
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn failure_tool_error_names_no_prelude_line() {
        let mut registry = Registry::new();
        registry.add(fake_replying("fail", object(), || {
            ToolOutput::error("no such file")
        }));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(&codemode, "print(1)\nprint(2)\nawait tools.fail({})").await;
        let line = error_line(&output);
        assert!(line.ends_with("no such file"), "{line}");
        assert!(
            !line.contains(" at line ") || line.contains(" at line 3:"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn failure_in_print_names_the_model_line() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "print(1)\nprint(2)\nprint(3)\nprint(4n)").await;
        let line = error_line(&output);
        assert!(line.starts_with("error: TypeError"), "{line}");
        assert!(
            !line.contains(" at line ") || line.contains(" at line 4:"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn failure_call_list_names_the_reason() {
        let mut registry = Registry::new();
        registry.add(fake_replying("fail", object(), || {
            ToolOutput::error("no such file: a.txt\nmore")
        }));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(&codemode, "try { await tools.fail({}) } catch (e) {}").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .lines()
                .any(|line| line == "- fail error: no such file: a.txt"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn slip_console_log_prints() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "console.log(\"hi\", 2)").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.lines().any(|line| line == "hi 2"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn slip_fence_is_removed() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "```js\nprint(1)\n```").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.lines().any(|line| line == "1"),
            "{}",
            output.content
        );
        assert!(output.content.contains(FENCE_NOTE), "{}", output.content);
    }

    #[tokio::test]
    async fn slip_printed_promise_is_named() {
        let mut registry = Registry::new();
        registry.add(fake_replying("t", object(), || ToolOutput::ok("done")));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(&codemode, "print(tools.t({}))").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.lines().any(|line| line == "[Promise]"),
            "{}",
            output.content
        );
        assert!(output.content.contains(PROMISE_NOTE), "{}", output.content);
    }

    #[tokio::test]
    async fn slip_empty_script_is_named() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "async function main() { print(1) }").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.starts_with("codemode completed in"),
            "{}",
            output.content
        );
        assert!(output.content.contains(NOTHING_NOTE), "{}", output.content);
        let printed = run_code(&codemode, "print(1)").await;
        assert!(!printed.is_error, "{}", printed.content);
        assert!(
            !printed.content.contains(NOTHING_NOTE),
            "{}",
            printed.content
        );
    }

    #[tokio::test]
    async fn run_return_value() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let object = run_code(&codemode, "return {a: 1}").await;
        assert!(!object.is_error, "{}", object.content);
        assert!(
            object.content.contains("return: {\"a\":1}"),
            "{}",
            object.content
        );
        let text = run_code(&codemode, "return \"hi\"").await;
        assert!(!text.is_error, "{}", text.content);
        assert!(text.content.contains("return: hi"), "{}", text.content);
    }

    #[tokio::test]
    async fn run_never_settles_fails_at_once() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let started = Instant::now();
        let output = run_code(&codemode, "await new Promise(() => {})").await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?}",
            started.elapsed()
        );
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("can never finish"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn run_unknown_tool_throws() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "await tools.nope({})").await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("unknown tool"),
            "{}",
            output.content
        );
        assert!(output.content.contains("nope"), "{}", output.content);
        assert!(
            output.content.contains("- nope error"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn run_ignores_extra_arguments() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let args = json!({ "code": "return 1", "note": 5 }).to_string();
        let output = codemode.run(&args, &CancellationToken::new()).await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("return: 1"), "{}", output.content);
    }

    #[tokio::test]
    async fn run_cancel_returns_at_once() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let args = json!({ "code": "await new Promise(() => {})" }).to_string();
        let started = Instant::now();
        let output = codemode.run(&args, &cancel).await;
        assert!(output.is_error);
        assert_eq!(output.content.as_ref(), "cancelled");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn guard_refused_call_throws_and_does_not_run() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let mut registry = Registry::new();
        registry.add(fake_replying("bash", with_path(), move || {
            counted.fetch_add(1, Ordering::SeqCst);
            ToolOutput::ok("ran")
        }));
        let codemode = Codemode::new(Arc::new(registry)).with_guard(deny_all_guard());
        let output = run_code(
            &codemode,
            "try { await tools.bash({command: \"ls\"}) } catch (e) { print(e.message) }",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the tool ran");
        assert!(output.content.contains("denied"), "{}", output.content);
        assert!(
            output.content.contains("- bash error"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn guard_skipped_tool_is_unknown_in_any_spelling() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let mut registry = Registry::new();
        registry.add(fake_replying("expand", object(), move || {
            counted.fetch_add(1, Ordering::SeqCst);
            ToolOutput::ok("")
        }));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(&codemode, "await tools.Expand({})").await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("unknown tool"),
            "{}",
            output.content
        );
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn guard_unknown_tool_names_it() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(&codemode, "await tools.nope({})").await;
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("nope"), "{}", output.content);
        assert!(
            output.content.contains("- nope error"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn guard_argument_types_are_coerced() {
        let mut registry = Registry::new();
        registry.add(fake_reading(
            "num",
            json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "required": ["n"]
            }),
            |args| {
                let value: Value = serde_json::from_str(args).unwrap_or_default();
                if value["n"].is_number() {
                    ToolOutput::ok(format!("n is {}", value["n"]))
                } else {
                    ToolOutput::error("invalid arguments: n")
                }
            },
        ));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(&codemode, "return await tools.num({n: \"5\"})").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("n is 5"), "{}", output.content);
    }

    #[tokio::test]
    async fn bash_object_script_reads_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add(reagent_tools::bash::Bash::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "const r = await tools.bash({command: \"echo hi; exit 3\"});\n\
             return [r.exit_code, r.output, r.truncated]",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("return: [3,\"hi\\n\",false]"),
            "{}",
            output.content
        );
    }

    /// A tool whose reply carries a numbered object over `text`.
    fn numbered(name: &str, text: &'static str) -> Fake {
        fake_replying(name, with_path(), move || {
            let mut object = Object::new(serde_json::Map::new()).numbered();
            object.fields.insert("start".into(), 1_u64.into());
            ToolOutput::ok(text).with_object(object)
        })
    }

    #[tokio::test]
    async fn read_object_sum_of_plain_lines() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("n.txt"), "1\n2\n3\n4\n5\n6\n7\n")
            .await
            .unwrap();
        let mut registry = Registry::new();
        registry.add(Read::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "const r = await tools.read({path: \"n.txt\"});\n\
             return r.output.split(\"\\n\").filter(Boolean).map(Number).reduce((a, b) => a + b, 0)",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("return: 28"), "{}", output.content);
    }

    #[tokio::test]
    async fn read_object_paging() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("n.txt"), "1\n2\n3\n4\n5\n")
            .await
            .unwrap();
        let mut registry = Registry::new();
        registry.add(Read::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "const r = await tools.read({path: \"n.txt\", limit: 2});\n\
             return [r.start, r.end, r.next_offset, r.output]",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("return: [1,2,3,\"1\\n2\\n\"]"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn read_object_notes_are_left_out() {
        let mut registry = Registry::new();
        registry.add(numbered(
            "notes",
            "[repaired: x]\n1\ta\n2\tb\n[3 more bytes; use offset=3]\n",
        ));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "const r = await tools.notes({path: \"a.txt\"});\nreturn r.output",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("return: a\nb\n"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn bash_object_rerouted_call_gets_read_text() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.txt"), "hello\n")
            .await
            .unwrap();
        let mut registry = Registry::new();
        registry.add(reagent_tools::bash::Bash::new(tmp.path()).with_reads(true));
        registry.add(Read::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "const r = await tools.bash({command: \"cat a.txt\"});\n\
             return typeof r === \"object\" && r.output === \"hello\\n\"",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("return: true"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn parallel_reads_overlap() {
        let track = Arc::new(Track::default());
        let mut registry = Registry::new();
        registry.add(tracked("slow", true, Duration::from_millis(200), &track));
        let codemode = Codemode::new(Arc::new(registry));
        let started = Instant::now();
        let output = run_code(
            &codemode,
            "await Promise.all([1, 2, 3, 4, 5, 6, 7, 8].map(() => tools.slow({})))",
        )
        .await;
        let elapsed = started.elapsed();
        assert!(!output.is_error, "{}", output.content);
        assert!(
            elapsed < Duration::from_millis(400),
            "eight read-only calls took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn parallel_cap_is_eight() {
        let track = Arc::new(Track::default());
        let mut registry = Registry::new();
        registry.add(tracked("tick", true, Duration::from_millis(50), &track));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "await Promise.all(Array.from({length: 20}, () => tools.tick({})))",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(track.peak(), 8, "the cap is not eight");
        assert_eq!(track.spans().len(), 20, "not every call ran");
    }

    #[tokio::test]
    async fn parallel_write_runs_alone() {
        let reads = Arc::new(Track::default());
        let writes = Arc::new(Track::default());
        let mut registry = Registry::new();
        registry.add(tracked("r", true, Duration::from_millis(100), &reads));
        registry.add(tracked("w", false, Duration::from_millis(100), &writes));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "await Promise.all([tools.r({}), tools.w({}), tools.r({})])",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let writes = writes.spans();
        assert_eq!(writes.len(), 1, "the writer ran {} times", writes.len());
        let (wrote_at, wrote_until) = writes[0];
        for (read_at, read_until) in reads.spans() {
            assert!(
                read_until <= wrote_at || wrote_until <= read_at,
                "the writer overlapped a read"
            );
        }
        assert!(
            output.content.contains("calls:\n- r ok\n- w ok\n- r ok"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn limit_endless_loop_hits_the_time_limit() {
        let codemode =
            Codemode::new(Arc::new(Registry::new())).with_wall_time(Duration::from_millis(200));
        let started = Instant::now();
        let output = run_code(&codemode, "while (true) {}").await;
        let elapsed = started.elapsed();
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("time limit"), "{}", output.content);
        assert!(
            output
                .content
                .lines()
                .next()
                .is_some_and(|line| line.starts_with("codemode failed in")),
            "{}",
            output.content
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn limit_51st_call_throws() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let mut registry = Registry::new();
        registry.add(fake_replying("t", object(), move || {
            counted.fetch_add(1, Ordering::SeqCst);
            ToolOutput::ok("ok")
        }));
        let codemode = Codemode::new(Arc::new(registry));
        let output = run_code(
            &codemode,
            "for (let i = 0; i < 51; i++) {\n\
             try { await tools.t({}) } catch (e) { print(e.message) }\n\
             }",
        )
        .await;
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("call limit"), "{}", output.content);
        assert_eq!(
            runs.load(Ordering::SeqCst),
            50,
            "the tool ran the wrong number of times"
        );
        assert!(output.content.contains("calls:\n"), "{}", output.content);
    }

    #[tokio::test]
    async fn limit_cancel_stops_a_loop() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let args = json!({ "code": "while (true) {}" }).to_string();
        let started = Instant::now();
        let output = codemode.run(&args, &cancel).await;
        let elapsed = started.elapsed();
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("cancelled"), "{}", output.content);
        assert!(
            elapsed < Duration::from_millis(1100),
            "the cancel took {elapsed:?}"
        );
    }

    #[test]
    fn limit_dropped_run_stops_the_engine() {
        // One blocking thread: a QuickJS thread left spinning blocks the probe.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let codemode = Codemode::new(Arc::new(Registry::new()));
            let cancel = CancellationToken::new();
            let args = json!({ "code": "while (true) {}" }).to_string();
            // A caller's own cancel `select!` drops the run future, as `run_one` does.
            tokio::select! {
                _ = codemode.run(&args, &cancel) => panic!("the loop ended"),
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            let probe = tokio::task::spawn_blocking(|| ());
            let ran = tokio::time::timeout(Duration::from_secs(2), probe).await;
            assert!(ran.is_ok(), "the QuickJS thread kept running");
        });
    }

    #[tokio::test]
    async fn limit_cancel_keeps_printed_lines() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let args = json!({ "code": "print(\"first\"); while (true) {}" }).to_string();
        let output = codemode.run(&args, &cancel).await;
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("first"), "{}", output.content);
        assert!(output.content.contains("cancelled"), "{}", output.content);
    }

    #[tokio::test]
    async fn limit_memory() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let started = Instant::now();
        let output = run_code(
            &codemode,
            "const a = [];\nwhile (true) a.push(new Array(1e6).fill(1))",
        )
        .await;
        let elapsed = started.elapsed();
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("memory limit"),
            "{}",
            output.content
        );
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    }

    /// The path the cut line of a result names.
    fn spill_path(text: &str) -> std::path::PathBuf {
        const AT: &str = "full output in ";
        let start = text.find(AT).expect("the cut names no file") + AT.len();
        let rest = &text[start..];
        let end = rest.find(']').expect("the cut line never ends");
        std::path::PathBuf::from(&rest[..end])
    }

    #[tokio::test]
    async fn spill_long_output_keeps_head_and_tail() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(
            &codemode,
            "for (let i = 0; i < 1000; i++) {\n\
             print(String(i).padStart(4, \"0\") + \" \" + \"x\".repeat(100))\n\
             }",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.contains("0000 x"), "{}", &text[..400]);
        assert!(text.contains("0999 x"), "{}", &text[text.len() - 400..]);
        assert!(text.contains("[output cut: "), "{text}");
        assert!(text.len() < 40 * 1024, "{} bytes in the result", text.len());
        let saved = tokio::fs::read_to_string(spill_path(text))
            .await
            .expect("the spill is there");
        assert_eq!(saved.lines().count(), 1000, "the spill is not whole");
        assert!(saved.contains("0000 x") && saved.contains("0999 x"));
    }

    #[tokio::test]
    async fn spill_short_output_is_whole() {
        let codemode = Codemode::new(Arc::new(Registry::new()));
        let output = run_code(
            &codemode,
            "for (let i = 0; i < 100; i++) {\n\
             print(String(i).padStart(4, \"0\") + \" \" + \"y\".repeat(95))\n\
             }",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(!text.contains("[output cut: "), "{text}");
        assert!(text.contains("0000 y"), "{text}");
        assert!(text.contains("0099 y"), "{text}");
        assert_eq!(text.lines().count(), 101, "the output is not whole");
    }

    #[tokio::test]
    async fn spill_runaway_print_is_capped() {
        let codemode =
            Codemode::new(Arc::new(Registry::new())).with_wall_time(Duration::from_secs(2));
        let started = Instant::now();
        let output = run_code(&codemode, "while (true) print(\"x\".repeat(1000))").await;
        let elapsed = started.elapsed();
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("print stopped"),
            "{}",
            &output.content[..output.content.len().min(200)]
        );
        assert!(
            output.content.contains("time limit"),
            "{}",
            &output.content[output.content.len() - 200..]
        );
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    }

    /// A registry of `names`, each tool with `with_path` arguments.
    fn hidden_registry(names: &[&str]) -> Arc<Registry> {
        let mut registry = Registry::new();
        for name in names {
            registry.add(fake(name, with_path()));
        }
        Arc::new(registry)
    }

    /// A source that hands out one echoing tool after a wait.
    struct Late {
        name: String,
        delay: Duration,
        reply: String,
    }

    impl Late {
        fn new(name: &str, delay: Duration, reply: &str) -> Self {
            Self {
                name: name.to_owned(),
                delay,
                reply: reply.to_owned(),
            }
        }
    }

    impl ToolSource for Late {
        fn resolve<'a>(
            &'a self,
            name: &'a str,
        ) -> BoxFuture<'a, Option<Result<Box<dyn Tool>, String>>> {
            Box::pin(async move {
                if name != self.name {
                    return if name.ends_with("__broken") {
                        Some(Err("the server said no".to_owned()))
                    } else {
                        None
                    };
                }
                tokio::time::sleep(self.delay).await;
                let reply = self.reply.clone();
                let tool = fake_replying(name, object(), move || ToolOutput::ok(reply.clone()));
                Some(Ok(Box::new(tool) as Box<dyn Tool>))
            })
        }

        fn names(&self) -> Vec<String> {
            vec![self.name.clone()]
        }
    }

    /// A source whose names and tools swap when it is refreshed.
    struct Swapping {
        names: Mutex<Vec<String>>,
        after: Vec<String>,
    }

    impl Swapping {
        /// A source that answers for `before` until it is refreshed, then for
        /// `after`.
        fn new(before: &[&str], after: &[&str]) -> Self {
            Self {
                names: Mutex::new(before.iter().map(|name| (*name).to_owned()).collect()),
                after: after.iter().map(|name| (*name).to_owned()).collect(),
            }
        }
    }

    impl ToolSource for Swapping {
        fn resolve<'a>(
            &'a self,
            name: &'a str,
        ) -> BoxFuture<'a, Option<Result<Box<dyn Tool>, String>>> {
            Box::pin(async move {
                if !self.after.iter().any(|current| current == name) {
                    return None;
                }
                let reply = format!("{name} ran");
                let tool = fake_replying(name, object(), move || ToolOutput::ok(reply.clone()));
                Some(Ok(Box::new(tool) as Box<dyn Tool>))
            })
        }

        fn names(&self) -> Vec<String> {
            self.names.lock().unwrap().clone()
        }

        fn refresh(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                *self.names.lock().unwrap() = self.after.clone();
            })
        }
    }

    /// The lines of `description` that hold a hidden signature.
    fn signatures(description: &str) -> Vec<&str> {
        description
            .lines()
            .filter(|line| line.starts_with("`tools.mcp__"))
            .collect()
    }

    #[test]
    fn mcp_codemode_lists_hidden_tools() {
        let mut registry = Registry::new();
        registry.add(fake(
            "mcp__gh__search",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}},
                "required": ["query"],
            }),
        ));
        registry.add(fake("mcp__gh__create", object()));
        registry.add(fake("mcp__gh__drop", object()).shape("object"));
        let hidden = vec![
            "mcp__gh__search".to_owned(),
            "mcp__gh__create".to_owned(),
            "mcp__gh__drop".to_owned(),
        ];
        let codemode = Codemode::new(Arc::new(registry)).with_hidden(hidden);
        let description = codemode.spec().description.as_str();
        let lines = signatures(description);
        assert_eq!(lines.len(), 3, "{description}");
        assert!(
            description.contains("`tools.mcp__gh__search({query, limit?})` -> string"),
            "{description}"
        );
        assert!(
            description.contains("`tools.mcp__gh__create({})` -> string"),
            "{description}"
        );
        // The shape of a hidden tool is in its signature line, once.
        assert!(
            description.contains("`tools.mcp__gh__drop({})` -> object"),
            "{description}"
        );
        assert!(!description.contains("returns object"), "{description}");
    }

    #[test]
    fn mcp_codemode_budget() {
        let names: Vec<String> = (0..500).map(|n| format!("mcp__gh__t{n}")).collect();
        let registry = hidden_registry(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let codemode = Codemode::new(registry).with_hidden(names);
        let description = codemode.spec().description.as_str();
        assert!(
            description.len() <= AGREED.len() + SIGNATURE_BUDGET + 1024,
            "{} bytes",
            description.len()
        );
        assert!(signatures(description).len() > 1, "{description}");
        assert!(
            description.contains("more tools; call listTools(\"mcp__gh\") and describeTool(name)"),
            "{}",
            &description[description.len() - 300..]
        );
    }

    #[tokio::test]
    async fn mcp_codemode_describe_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        registry.add(fake("mcp__a_echo", with_path()));
        registry.add(fake("mcp__a_two", object()));
        registry.add(Read::new(tmp.path()));
        let codemode = Codemode::new(Arc::new(registry))
            .with_hidden(vec!["mcp__a_echo".to_owned(), "mcp__a_two".to_owned()]);
        let output = run_code(
            &codemode,
            "const d = describeTool(\"mcp__a_echo\");\n\
             print(d.name, d.description, JSON.stringify(d.inputSchema));\n\
             print(String(describeTool(\"nope\")));\n\
             print(listTools(\"mcp__a\").join(\",\"));\n\
             print(listTools().length)",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.contains("mcp__a_echo A test tool."), "{text}");
        assert!(text.contains("\"required\":[\"path\"]"), "{text}");
        assert!(text.contains("\nnull\n"), "{text}");
        assert!(text.contains("\nmcp__a_echo,mcp__a_two\n"), "{text}");
        assert!(text.contains("\n3\n"), "{text}");
    }

    #[tokio::test]
    async fn mcp_codemode_describe_output_schema() {
        let schema = json!({"type": "object", "properties": {"total": {"type": "number"}}});
        let mut registry = Registry::new();
        registry.add(fake("mcp__a_total", object()).output_schema(schema.clone()));
        registry.add(fake("mcp__a_plain", object()));
        let codemode = Codemode::new(Arc::new(registry))
            .with_hidden(vec!["mcp__a_total".to_owned(), "mcp__a_plain".to_owned()]);
        let output = run_code(
            &codemode,
            "print(JSON.stringify(describeTool(\"mcp__a_total\").outputSchema));\n\
             print(\"outputSchema\" in describeTool(\"mcp__a_plain\"));",
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(
            text.contains(&serde_json::to_string(&schema).unwrap()),
            "{text}"
        );
        assert!(text.contains("\nfalse\n"), "{text}");
    }

    #[tokio::test]
    async fn mcp_codemode_refresh_before_script() {
        let source = Swapping::new(&["mcp__late__a"], &["mcp__late__b"]);
        let codemode = Codemode::new(hidden_registry(&["mcp__late__a"]))
            .with_hidden(vec!["mcp__late__a".to_owned()])
            .with_source(Arc::new(source) as Arc<dyn ToolSource>);
        let output = run_code(
            &codemode,
            "print(listTools(\"mcp__\").join(\",\"));\n\
             print(await tools.mcp__late__b({}));\n\
             try { await tools.mcp__late__a({}); } catch (error) { print(\"unknown\", error.message); }",
        )
        .await;
        let text = output.content.as_ref();
        assert!(text.contains("\nmcp__late__b\n"), "{text}");
        assert!(!text.contains("\nmcp__late__a\n"), "{text}");
        assert!(text.contains("mcp__late__b ran"), "{text}");
        assert!(
            text.contains("unknown unknown tool `mcp__late__a`"),
            "{text}"
        );
    }

    /// A source whose list changes only when its generation does.
    struct Counted {
        name: String,
        generation: AtomicU64,
        /// How many times its data was built, which is one `names` call each.
        builds: AtomicUsize,
    }

    impl Counted {
        /// A source that answers for `name` at generation one.
        fn new(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                generation: AtomicU64::new(1),
                builds: AtomicUsize::new(0),
            }
        }

        fn builds(&self) -> usize {
            self.builds.load(Ordering::SeqCst)
        }

        /// Says that its tool list changed.
        fn changed(&self) {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ToolSource for Counted {
        fn resolve<'a>(
            &'a self,
            name: &'a str,
        ) -> BoxFuture<'a, Option<Result<Box<dyn Tool>, String>>> {
            Box::pin(async move {
                (name == self.name).then(|| {
                    let reply = format!("{name} ran");
                    let tool = fake_replying(name, object(), move || ToolOutput::ok(reply.clone()));
                    Ok(Box::new(tool) as Box<dyn Tool>)
                })
            })
        }

        fn names(&self) -> Vec<String> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            vec![self.name.clone()]
        }

        fn generation(&self) -> u64 {
            self.generation.load(Ordering::SeqCst)
        }
    }

    /// The data the codemode holds for the current generation of its source.
    fn held(codemode: &Codemode) -> Arc<[Described]> {
        let listed = codemode.listed_mutex();
        let listed = listed.as_ref().expect("no data was kept");
        Arc::clone(&listed.described)
    }

    #[tokio::test]
    async fn mcp_codemode_described_reused() {
        let source = Arc::new(Counted::new("mcp__late__echo"));
        let codemode = Codemode::new(Arc::new(Registry::new()))
            .with_source(Arc::clone(&source) as Arc<dyn ToolSource>);
        let code = "print(listTools(\"mcp__\").join(\",\"));";
        let first = run_code(&codemode, code).await;
        assert!(
            first.content.contains("\nmcp__late__echo\n"),
            "{}",
            first.content
        );
        assert_eq!(source.builds(), 1, "the first script built no data");
        let kept = held(&codemode);
        let second = run_code(&codemode, code).await;
        assert!(
            second.content.contains("\nmcp__late__echo\n"),
            "{}",
            second.content
        );
        assert!(
            Arc::ptr_eq(&kept, &held(&codemode)),
            "the second script built the data again"
        );
        assert_eq!(source.builds(), 1, "the second script built the data again");

        source.changed();
        let third = run_code(&codemode, code).await;
        assert!(
            third.content.contains("\nmcp__late__echo\n"),
            "{}",
            third.content
        );
        assert_eq!(source.builds(), 2, "the changed list was not built once");
        assert!(
            !Arc::ptr_eq(&kept, &held(&codemode)),
            "the changed list kept the old data"
        );
        run_code(&codemode, code).await;
        assert_eq!(source.builds(), 2, "the data was built with no change");
    }

    #[tokio::test]
    async fn mcp_codemode_changed_spec_comes_from_source() {
        // A hidden tool whose server relisted it keeps its name but takes the
        // spec and the adapter the source hands out now.
        let source = Arc::new(Counted::new("mcp__late__echo"));
        let codemode = Codemode::new(hidden_registry(&["mcp__late__echo"]))
            .with_hidden(vec!["mcp__late__echo".to_owned()])
            .with_source(Arc::clone(&source) as Arc<dyn ToolSource>);
        let code = "print(JSON.stringify(describeTool(\"mcp__late__echo\").inputSchema));\n\
                    print(await tools.mcp__late__echo({path: \"a\"}));";
        let before = run_code(&codemode, code).await;
        assert!(
            before.content.contains("\"required\""),
            "{}",
            before.content
        );
        assert!(!before.content.contains("ran"), "{}", before.content);
        source.changed();
        let after = run_code(&codemode, code).await;
        assert!(
            after.content.contains("{\"type\":\"object\"}"),
            "{}",
            after.content
        );
        assert!(
            after.content.contains("mcp__late__echo ran"),
            "{}",
            after.content
        );
    }

    #[tokio::test]
    async fn mcp_codemode_source_resolves_late_tool() {
        let source = Late::new("mcp__late__echo", Duration::from_millis(50), "late ok");
        let codemode = Codemode::new(Arc::new(Registry::new()))
            .with_source(Arc::new(source) as Arc<dyn ToolSource>);
        let output = run_code(
            &codemode,
            "const text = await tools.mcp__late__echo({text: \"hi\"});\n\
             print(text);\n\
             try { await tools.mcp__late__broken({}); } catch (error) { print(\"threw\", error.message); }\n\
             try { await tools.mcp__late__nope({}); } catch (error) { print(\"unknown\", error.message); }",
        )
        .await;
        let text = output.content.as_ref();
        assert!(text.contains("late ok"), "{text}");
        assert!(text.contains("threw the server said no"), "{text}");
        assert!(text.contains("unknown unknown tool"), "{text}");
        assert!(text.contains("- mcp__late__echo ok"), "{text}");
    }

    #[tokio::test]
    async fn mcp_codemode_no_hidden_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = {
            let mut registry = Registry::new();
            registry.add(Read::new(tmp.path()));
            registry.add(reagent_tools::bash::Bash::new(tmp.path()));
            Codemode::new(Arc::new(registry))
        };
        let same = {
            let mut registry = Registry::new();
            registry.add(Read::new(tmp.path()));
            registry.add(reagent_tools::bash::Bash::new(tmp.path()));
            Codemode::new(Arc::new(registry)).with_hidden(Vec::new())
        };
        assert_eq!(
            plain.spec().description,
            same.spec().description,
            "a run with no hidden tool adds no line"
        );
        let code = "await tools.read({path: \"/nope\"})";
        let first = run_code(&plain, code).await;
        let second = run_code(&same, code).await;
        let body = |text: &str| text.split_once('\n').map(|(_, body)| body.to_owned());
        assert!(first.is_error, "{}", first.content);
        assert_eq!(body(&first.content), body(&second.content));
    }
}
