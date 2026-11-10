//! The one thread every plugin runs on: a QuickJS runtime, its context, and
//! the persistent handles of what the plugins registered.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::{
    Context, Ctx, Error, Exception, Function, IntoJs, Module, Object, Persistent, Promise,
    Result as JsResult, Runtime, Value,
};
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;

use super::api::{self, Host, Slot, Staging};
use super::manifest::{self, Found, Manifest};
use super::{CommandInfo, Live, Notes};

/// The most bytes every plugin together may allocate.
const MEMORY_CAP: usize = 256 * 1024 * 1024;

/// How long one JS call may run before a note says it is slow.
const SLOW: Duration = Duration::from_secs(1);

/// How long one JS call may run before the runtime stops it.
const STOP: Duration = Duration::from_secs(5);

/// The deadline that means no JS call is running.
const IDLE: u64 = u64::MAX;

/// What a plugin call that did not answer says when it was cut short.
const STOPPED: &str = "the plugin call was stopped after 5 s";

/// What a plugin waits on that only a promise could settle says.
const NEVER: &str = "the promise never settles: plugins have no timers or I/O";

/// One request the async side makes of the plugin thread.
pub(crate) enum Job {
    /// Loads every plugin under this root.
    Load {
        /// The folder to read.
        root: PathBuf,
        /// Carries the report back.
        reply: oneshot::Sender<LoadReport>,
    },
    /// Runs one plugin tool.
    Run {
        /// The tool's name.
        tool: String,
        /// Its arguments, as JSON text.
        args: String,
        /// Carries the result back.
        reply: oneshot::Sender<RunReply>,
    },
    /// Runs one slash command.
    Command {
        /// The command's name, without the slash.
        name: String,
        /// The rest of the user's line, as one string.
        args: String,
        /// Carries the text to show back, if any.
        reply: oneshot::Sender<Option<String>>,
    },
    /// Runs the `tool.call` chain of one call.
    Call {
        /// The tool's name.
        tool: String,
        /// Its arguments, as JSON text.
        args: String,
        /// Carries the verdict back.
        reply: oneshot::Sender<CallReply>,
    },
    /// Runs the `tool.result` chain of one call.
    Result {
        /// The tool's name.
        tool: String,
        /// Its arguments, as JSON text.
        args: String,
        /// The text the call produced.
        text: String,
        /// Whether the call failed.
        is_error: bool,
        /// Carries the rewritten result back.
        reply: oneshot::Sender<ResultReply>,
    },
    /// Runs the `turn.end` chain of one turn.
    Turn {
        /// The 1-based index of the turn.
        index: u32,
        /// Whether the turn ran a tool.
        ran_tools: bool,
        /// The assistant reply's text.
        text: String,
        /// Carries the follow-up texts back.
        reply: oneshot::Sender<Vec<String>>,
    },
    /// Hands one event to every listener waiting on it.
    Event {
        /// The event's name.
        name: String,
        /// What the listener sees.
        payload: JsonValue,
    },
}

/// What loading reported: the tools to offer, the commands to list and which
/// chains exist.
#[derive(Default)]
pub(crate) struct LoadReport {
    /// The tools the plugins registered, in load order.
    pub tools: Vec<ToolInfo>,
    /// The commands the plugins registered, in load order.
    pub commands: Vec<CommandInfo>,
    /// How many plugins loaded.
    pub loaded: usize,
    /// Whether any plugin installed a `tool.call` layer.
    pub call: bool,
    /// Whether any plugin installed a `tool.result` layer.
    pub result: bool,
    /// Whether any plugin installed a `turn.end` layer.
    pub turn: bool,
    /// A bit per event some listener waits on.
    pub events: u8,
}

/// One registered tool, as the async side offers it to the model.
pub(crate) struct ToolInfo {
    /// The name the model calls.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: JsonValue,
}

/// What one run of a plugin tool produced.
pub(crate) struct RunReply {
    /// The text the model sees.
    pub text: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// What the `tool.call` chain decided.
pub(crate) enum CallReply {
    /// Run the call with its arguments as they are.
    Keep,
    /// Run the call with these arguments, as JSON text.
    Args(String),
    /// Do not run the call, for this reason.
    Block(String),
}

/// What the `tool.result` chain made of a call's output: only what a layer
/// replaced, so the rest of the output reaches the model as it was.
pub(crate) struct ResultReply {
    /// The text a layer replaced, if any did.
    pub text: Option<String>,
    /// Whether a layer replaced the error flag, if any did.
    pub is_error: Option<bool>,
}

/// Why one JS call did not answer.
enum Failed {
    /// It threw, with this message.
    Threw(String),
    /// The watchdog stopped it, and a note already says so.
    Stopped,
}

/// Starts the plugin thread, with the notes the plugins raise.
pub(crate) fn start(notes: Arc<Mutex<Notes>>) -> Option<(SyncSender<Job>, Arc<Live>)> {
    let wake = match notes.lock() {
        Ok(notes) => notes.wake(),
        Err(broken) => broken.into_inner().wake(),
    };
    let live = Arc::new(Live::new(wake, Arc::clone(&notes)));
    let (jobs, queue) = sync_channel(super::JOBS);
    let hosting = Arc::clone(&live);
    std::thread::Builder::new()
        .name("plugin".into())
        .spawn(move || host(queue, notes, hosting))
        .ok()?;
    Some((jobs, live))
}

/// The thread's whole life: one runtime, one context, one job at a time.
fn host(queue: Receiver<Job>, notes: Arc<Mutex<Notes>>, live: Arc<Live>) {
    let Ok(runtime) = Runtime::new() else {
        note(
            &notes,
            "plugins: the JavaScript runtime did not start".into(),
        );
        return;
    };
    runtime.set_memory_limit(MEMORY_CAP);
    let watch = Arc::new(Watch::new());
    runtime.set_interrupt_handler(Some(watch.handler()));
    let folders = Arc::new(Mutex::new(HashSet::new()));
    let loader = PluginLoader {
        folders: Arc::clone(&folders),
    };
    runtime.set_loader(loader.clone(), loader);
    let Ok(context) = Context::full(&runtime) else {
        note(
            &notes,
            "plugins: the JavaScript context did not start".into(),
        );
        return;
    };
    let mut engine = Engine::new();
    while let Ok(job) = queue.recv() {
        let notes = Arc::clone(&notes);
        context.with(|ctx| handle(&ctx, &mut engine, &watch, &notes, &live, &folders, job));
    }
}

/// Answers one job.
fn handle(
    ctx: &Ctx<'_>,
    engine: &mut Engine,
    watch: &Arc<Watch>,
    notes: &Arc<Mutex<Notes>>,
    live: &Arc<Live>,
    folders: &Mutex<HashSet<PathBuf>>,
    job: Job,
) {
    match job {
        Job::Load { root: dir, reply } => {
            let report = load_all(ctx, engine, &dir, watch, notes, live, folders);
            let _ = reply.send(report);
        }
        Job::Run { tool, args, reply } => {
            let out = run_tool(ctx, engine, watch, notes, &tool, &args);
            let _ = reply.send(out);
        }
        Job::Command { name, args, reply } => {
            let out = run_command(ctx, engine, watch, notes, &name, &args);
            let _ = reply.send(out);
        }
        Job::Call { tool, args, reply } => {
            let out = chain_call(ctx, engine, watch, notes, &tool, &args);
            let _ = reply.send(out);
        }
        Job::Result {
            tool,
            args,
            text,
            is_error,
            reply,
        } => {
            let out = chain_result(ctx, engine, watch, notes, &tool, &args, (text, is_error));
            let _ = reply.send(out);
        }
        Job::Turn {
            index,
            ran_tools,
            text,
            reply,
        } => {
            let out = chain_turn(ctx, engine, watch, notes, index, ran_tools, &text);
            let _ = reply.send(out);
        }
        Job::Event { name, payload } => fire(ctx, engine, watch, notes, &name, &payload),
    }
}

/// The interrupt handler's clock: one deadline for the JS call running now.
struct Watch {
    /// When the runtime started, which every deadline counts from.
    start: Instant,
    /// When the running call began, or 0 when no call began yet.
    began: AtomicU64,
    /// When the running call is interrupted, or [`IDLE`].
    deadline: AtomicU64,
    /// Whether the running call passed [`SLOW`].
    slow: AtomicBool,
    /// Whether the handler stopped the running call.
    fired: AtomicBool,
}

impl Watch {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            began: AtomicU64::new(0),
            deadline: AtomicU64::new(IDLE),
            slow: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        }
    }

    /// The handler QuickJS calls while it runs JS.
    fn handler(self: &Arc<Self>) -> Box<dyn FnMut() -> bool + 'static> {
        let watch = Arc::clone(self);
        Box::new(move || watch.check())
    }

    /// Notes that the running call is slow, and stops it past its deadline.
    fn check(&self) -> bool {
        let deadline = self.deadline.load(Ordering::Relaxed);
        if deadline == IDLE {
            return false;
        }
        let now = self.elapsed();
        if now >= deadline {
            self.fired.store(true, Ordering::Relaxed);
            return true;
        }
        // The call's own start, not the deadline: the note is due at 1 s.
        if now >= self.began.load(Ordering::Relaxed) + SLOW.as_nanos() as u64 {
            self.slow.store(true, Ordering::Relaxed);
        }
        false
    }

    /// How long the runtime has run, in nanoseconds.
    fn elapsed(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// Arms the deadline of the call that is about to run.
    fn begin(&self) {
        self.slow.store(false, Ordering::Relaxed);
        self.fired.store(false, Ordering::Relaxed);
        let began = self.elapsed();
        self.began.store(began, Ordering::Relaxed);
        self.deadline
            .store(began + STOP.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Disarms the deadline and reports whether the call was slow or stopped.
    fn end(&self) -> (bool, bool) {
        self.deadline.store(IDLE, Ordering::Relaxed);
        (
            self.slow.load(Ordering::Relaxed),
            self.fired.load(Ordering::Relaxed),
        )
    }
}

/// Runs one JS call under the watchdog, with `what` naming it in a note.
fn watched<T>(
    ctx: &Ctx<'_>,
    watch: &Arc<Watch>,
    notes: &Mutex<Notes>,
    plugin: &str,
    what: &str,
    call: impl FnOnce() -> Result<T, String>,
) -> Result<T, Failed> {
    watch.begin();
    let out = call();
    let (slow, fired) = watch.end();
    if fired {
        // The interrupt left the thrown error in the context; take it out.
        ctx.catch();
        note(notes, format!("plugin {plugin}: {what} stopped after 5 s"));
        return Err(Failed::Stopped);
    }
    if slow {
        note(notes, format!("plugin {plugin}: {what} is slow"));
    }
    out.map_err(Failed::Threw)
}

/// Appends one note for [`super::Plugins::take_notes`].
pub(super) fn note(notes: &Mutex<Notes>, text: String) {
    if let Ok(mut notes) = notes.lock() {
        notes.push(text);
    }
}

/// The plugins that loaded and everything they registered.
struct Engine {
    /// Every tool name taken, so a duplicate throws at load.
    names: Rc<RefCell<HashSet<String>>>,
    /// Every command name taken, so a duplicate is refused.
    command_names: Rc<RefCell<HashSet<String>>>,
    /// The tools, in load order.
    tools: Vec<Registered>,
    /// The commands, in load order.
    commands: Vec<RegisteredCommand>,
    /// The `tool.call` layers, in load order.
    call: Vec<Layer>,
    /// The `tool.result` layers, in load order.
    result: Vec<Layer>,
    /// The `turn.end` layers, in load order.
    turn: Vec<Layer>,
    /// The listeners, in load order.
    listeners: Vec<Listener>,
}

/// One tool a plugin registered, with the plugin that owns it.
struct Registered {
    /// The name the model calls.
    name: String,
    /// The plugin that registered it.
    plugin: String,
    /// The function that runs it.
    run: Persistent<Function<'static>>,
}

/// One command a plugin registered, with the plugin that owns it.
struct RegisteredCommand {
    /// The name the user types after the slash.
    name: String,
    /// The plugin that registered it.
    plugin: String,
    /// The function that runs it.
    run: Persistent<Function<'static>>,
}

/// One hook layer, with the plugin that installed it.
struct Layer {
    /// The plugin that installed it.
    plugin: String,
    /// The function the layer runs.
    func: Persistent<Function<'static>>,
}

/// One event listener, with the plugin that registered it.
struct Listener {
    /// The plugin that registered it.
    plugin: String,
    /// The event it waits on.
    event: String,
    /// The function the listener runs.
    func: Persistent<Function<'static>>,
}

impl Engine {
    fn new() -> Self {
        Self {
            names: Rc::new(RefCell::new(HashSet::new())),
            command_names: Rc::new(RefCell::new(HashSet::new())),
            tools: Vec::new(),
            commands: Vec::new(),
            call: Vec::new(),
            result: Vec::new(),
            turn: Vec::new(),
            listeners: Vec::new(),
        }
    }
}

/// Reads every plugin folder under `root` and loads them in folder order.
fn load_all(
    ctx: &Ctx<'_>,
    engine: &mut Engine,
    root: &Path,
    watch: &Arc<Watch>,
    notes: &Arc<Mutex<Notes>>,
    live: &Arc<Live>,
    folders: &Mutex<HashSet<PathBuf>>,
) -> LoadReport {
    let mut report = LoadReport::default();
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread, off the async runtime"
    )]
    let Ok(entries) = std::fs::read_dir(root) else {
        return report;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    // Each name loaded so far and the folder it came from: the manifest name
    // is what panels are prefixed with, so two plugins may not share it.
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for dir in dirs {
        let folder = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match manifest::read(&dir) {
            Found::Skipped => {}
            Found::Bad(reason) => note(notes, format!("plugin {folder}: {reason}")),
            Found::Plugin(manifest) => {
                if let Some((_, taken)) = names.iter().find(|(name, _)| *name == manifest.name) {
                    note(
                        notes,
                        format!(
                            "plugin {folder}: the name '{}' is already taken by the plugin at {}",
                            manifest.name,
                            taken.display()
                        ),
                    );
                    continue;
                }
                match load_one(ctx, engine, &manifest, watch, notes, live, folders) {
                    Ok(owned) => {
                        names.push((manifest.name.clone(), dir));
                        report.loaded += 1;
                        report.tools.extend(owned.tools);
                        report.commands.extend(owned.commands);
                    }
                    Err(Failed::Threw(reason)) => {
                        note(notes, format!("plugin {folder}: {reason}"));
                    }
                    Err(Failed::Stopped) => {}
                }
            }
        }
    }
    report.call = !engine.call.is_empty();
    report.result = !engine.result.is_empty();
    report.turn = !engine.turn.is_empty();
    report.events = engine
        .listeners
        .iter()
        .filter_map(|listener| api::event_bit(&listener.event))
        .fold(0, |bits, bit| bits | bit);
    report
}

/// What one plugin registered, for the load report.
struct Owned {
    /// The tools it registered.
    tools: Vec<ToolInfo>,
    /// The commands it registered.
    commands: Vec<CommandInfo>,
}

/// Runs one plugin's entry, then commits what it registered.
fn load_one(
    ctx: &Ctx<'_>,
    engine: &mut Engine,
    manifest: &Manifest,
    watch: &Arc<Watch>,
    notes: &Arc<Mutex<Notes>>,
    live: &Arc<Live>,
    folders: &Mutex<HashSet<PathBuf>>,
) -> Result<Owned, Failed> {
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread, off the async runtime"
    )]
    let source = std::fs::read_to_string(&manifest.entry)
        .map_err(|error| Failed::Threw(format!("{}: {error}", manifest.entry.display())))?;
    // Every plugin's own folder, so its imports resolve inside it and not in
    // the folder of the plugin that happened to load last.
    if let Ok(mut folders) = folders.lock() {
        folders.insert(manifest.dir.clone());
    }
    let staging = Rc::new(RefCell::new(Staging::default()));
    // Set once the load returned: from then on a panel the plugin makes is
    // live at once, because this commit is the only one it gets.
    let committed = Rc::new(Cell::new(false));
    let name = manifest.entry.to_string_lossy().into_owned();
    let host = Host {
        notes: Arc::clone(notes),
        live: Arc::clone(live),
        taken: Rc::clone(&engine.command_names),
        committed: Rc::clone(&committed),
    };
    watched(ctx, watch, notes, &manifest.name, "load", || {
        let reagent = api::reagent(
            ctx,
            &manifest.name,
            &manifest.version,
            &staging,
            &engine.names,
            &host,
        )
        .map_err(|error| error.to_string())?;
        let module =
            Module::declare(ctx.clone(), name, source).map_err(|error| failure(ctx, error))?;
        let (module, promise) = module.eval().map_err(|error| failure(ctx, error))?;
        settle_promise(ctx, promise)?;
        let entry = module
            .get::<_, Option<Function>>("default")
            .map_err(|error| failure(ctx, error))?
            .ok_or_else(|| "the entry has no default export".to_string())?;
        call1(ctx, &entry, reagent)?;
        Ok(())
    })?;
    let mut staged = std::mem::take(&mut *staging.borrow_mut());
    let mut tools = Vec::with_capacity(staged.tools.len());
    for tool in staged.tools.drain(..) {
        engine.names.borrow_mut().insert(tool.name.clone());
        tools.push(ToolInfo {
            name: tool.name.clone(),
            description: tool.description,
            parameters: tool.parameters,
        });
        engine.tools.push(Registered {
            name: tool.name,
            plugin: manifest.name.clone(),
            run: tool.run,
        });
    }
    let mut commands = Vec::with_capacity(staged.commands.len());
    for command in staged.commands.drain(..) {
        engine
            .command_names
            .borrow_mut()
            .insert(command.name.clone());
        commands.push(CommandInfo {
            name: command.name.clone(),
            description: command.description,
        });
        engine.commands.push(RegisteredCommand {
            name: command.name,
            plugin: manifest.name.clone(),
            run: command.run,
        });
    }
    // The panels were made while the plugin loaded; only now does the host see
    // them, and only now do their `set` calls reach it.
    let slots: Vec<Rc<RefCell<Slot>>> = staged.panels.borrow().clone();
    for slot in slots {
        let mut slot = slot.borrow_mut();
        live.add_panel(slot.id.clone(), slot.title.clone(), slot.lines.clone());
        slot.live = Some(Arc::clone(live));
    }
    committed.set(true);
    let layer = |func| Layer {
        plugin: manifest.name.clone(),
        func,
    };
    engine.call.extend(staged.call.into_iter().map(layer));
    engine.result.extend(staged.result.into_iter().map(layer));
    engine.turn.extend(staged.turn.into_iter().map(layer));
    engine
        .listeners
        .extend(staged.listeners.into_iter().map(|(event, func)| Listener {
            plugin: manifest.name.clone(),
            event,
            func,
        }));
    Ok(Owned { tools, commands })
}

/// Runs one slash command and turns whatever it returned into the text to
/// show.
fn run_command(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Arc<Watch>,
    notes: &Mutex<Notes>,
    name: &str,
    args: &str,
) -> Option<String> {
    let Some(registered) = engine.commands.iter().find(|entry| entry.name == name) else {
        return Some(format!("plugins: no command is named '/{name}'"));
    };
    let plugin = registered.plugin.as_str();
    let Some(func) = restore(ctx, &registered.run) else {
        return Some(format!("plugin {plugin}: command /{name}: {STOPPED}"));
    };
    let what = format!("command {name}");
    let out = watched(ctx, watch, notes, plugin, &what, || {
        let value = args.into_js(ctx).map_err(|error| failure(ctx, error))?;
        let value = call1(ctx, &func, value)?;
        shown(ctx, value)
    });
    match out {
        Ok(text) => text,
        Err(Failed::Threw(reason)) => {
            Some(format!("plugin {plugin}: command /{name} threw: {reason}"))
        }
        Err(Failed::Stopped) => Some(format!(
            "plugin {plugin}: command /{name} stopped after 5 s"
        )),
    }
}

/// The text a command's return value stands for; no value shows nothing.
fn shown<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Option<String>, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return Ok(Some(text));
    }
    // A number, an object or anything else is shown as `String(value)`.
    let to_text: Function = ctx
        .globals()
        .get("String")
        .map_err(|error| failure(ctx, error))?;
    to_text
        .call::<_, String>((value,))
        .map(Some)
        .map_err(|error| failure(ctx, error))
}

/// Runs one plugin tool and turns whatever it returned into a result.
fn run_tool(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Arc<Watch>,
    notes: &Mutex<Notes>,
    tool: &str,
    args: &str,
) -> RunReply {
    let Some(registered) = engine.tools.iter().find(|entry| entry.name == tool) else {
        return RunReply {
            text: format!("no plugin tool is named '{tool}'"),
            is_error: true,
        };
    };
    let Some(func) = restore(ctx, &registered.run) else {
        return stopped();
    };
    let parsed = args_object(args);
    let out = watched(
        ctx,
        watch,
        notes,
        &registered.plugin,
        &format!("tool {tool}"),
        || {
            let arguments = api::from_json(ctx, &parsed).map_err(|error| error.to_string())?;
            let context = Object::new(ctx.clone()).map_err(|error| error.to_string())?;
            context
                .set("tool", tool)
                .and_then(|_| context.set("plugin", registered.plugin.as_str()))
                .map_err(|error| error.to_string())?;
            let value = call2(ctx, &func, arguments, context.into_value())?;
            Ok(answered(ctx, value))
        },
    );
    match out {
        Ok(reply) => reply,
        Err(Failed::Threw(text)) => RunReply {
            text,
            is_error: true,
        },
        Err(Failed::Stopped) => stopped(),
    }
}

/// The result a plugin tool's return value stands for.
fn answered<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> RunReply {
    if value.is_undefined() || value.is_null() {
        return RunReply {
            text: String::new(),
            is_error: false,
        };
    }
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return RunReply {
            text,
            is_error: false,
        };
    }
    if let Some(object) = value.as_object()
        && let Ok(Some(text)) = object.get::<_, Option<String>>("text")
    {
        let is_error = object
            .get::<_, Option<bool>>("isError")
            .ok()
            .flatten()
            .unwrap_or(false);
        return RunReply { text, is_error };
    }
    RunReply {
        text: api::json_of(ctx, value)
            .map(|json| json.to_string())
            .unwrap_or_default(),
        is_error: false,
    }
}

/// The result a call reports when the runtime could not run it.
fn stopped() -> RunReply {
    RunReply {
        text: STOPPED.into(),
        is_error: true,
    }
}

/// Runs every `tool.call` layer in load order, each on the previous value.
fn chain_call(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Arc<Watch>,
    notes: &Mutex<Notes>,
    tool: &str,
    args: &str,
) -> CallReply {
    let mut current = args_object(args);
    let mut changed = false;
    for layer in &engine.call {
        let Some(func) = restore(ctx, &layer.func) else {
            continue;
        };
        let out = watched(ctx, watch, notes, &layer.plugin, "hook tool.call", || {
            let payload = Object::new(ctx.clone()).map_err(|error| error.to_string())?;
            payload
                .set("tool", tool)
                .and_then(|_| payload.set("args", api::from_json(ctx, &current)?))
                .map_err(|error| error.to_string())?;
            let value = call1(ctx, &func, payload.into_value())?;
            Ok(step(ctx, &layer.plugin, value))
        });
        match out {
            Ok(Step::Keep) => {}
            Ok(Step::Args(next)) => {
                current = next;
                changed = true;
            }
            Ok(Step::Block(reason)) => return CallReply::Block(reason),
            Err(Failed::Threw(message)) => note(
                notes,
                format!("plugin {}: hook tool.call threw: {message}", layer.plugin),
            ),
            Err(Failed::Stopped) => {}
        }
    }
    if changed {
        CallReply::Args(current.to_string())
    } else {
        CallReply::Keep
    }
}

/// What one `tool.call` layer made of a call.
enum Step {
    /// Keep the arguments as they are.
    Keep,
    /// Use these arguments instead.
    Args(JsonValue),
    /// Do not run the call, for this reason.
    Block(String),
}

/// The step one `tool.call` layer's return value stands for.
///
/// Only a reason or `true` blocks: `{ block: false }` is not a block, and a
/// layer that returns one keeps the call.
fn step<'js>(ctx: &Ctx<'js>, plugin: &str, value: Value<'js>) -> Step {
    let Some(object) = value.as_object() else {
        return Step::Keep;
    };
    if let Ok(Some(block)) = object.get::<_, Option<Value>>("block") {
        if let Some(reason) = block.as_string().and_then(|text| text.to_string().ok()) {
            return Step::Block(reason);
        }
        if block.as_bool() == Some(true) {
            return Step::Block(format!("blocked by plugin {plugin}"));
        }
    }
    if let Ok(Some(args)) = object.get::<_, Option<Value>>("args")
        && args.is_object()
        && let Some(json) = api::json_of(ctx, args)
    {
        return Step::Args(json);
    }
    Step::Keep
}

/// Runs every `tool.result` layer in load order, each on the previous value.
fn chain_result(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Arc<Watch>,
    notes: &Mutex<Notes>,
    tool: &str,
    args: &str,
    first: (String, bool),
) -> ResultReply {
    let parsed = args_object(args);
    let (mut text, mut is_error) = first;
    let mut said = (None, None);
    for layer in &engine.result {
        let Some(func) = restore(ctx, &layer.func) else {
            continue;
        };
        let out = watched(ctx, watch, notes, &layer.plugin, "hook tool.result", || {
            let payload = Object::new(ctx.clone()).map_err(|error| error.to_string())?;
            payload
                .set("tool", tool)
                .and_then(|_| payload.set("args", api::from_json(ctx, &parsed)?))
                .and_then(|_| payload.set("text", text.as_str()))
                .and_then(|_| payload.set("isError", is_error))
                .map_err(|error| error.to_string())?;
            let value = call1(ctx, &func, payload.into_value())?;
            // Reading the answer runs getters, so it stays under the watchdog.
            Ok(rewritten(value))
        });
        match out {
            Ok((next_text, next_error)) => {
                if let Some(next) = next_text {
                    text = next.clone();
                    said.0 = Some(next);
                }
                if let Some(next) = next_error {
                    is_error = next;
                    said.1 = Some(next);
                }
            }
            Err(Failed::Threw(message)) => note(
                notes,
                format!("plugin {}: hook tool.result threw: {message}", layer.plugin),
            ),
            Err(Failed::Stopped) => {}
        }
    }
    ResultReply {
        text: said.0,
        is_error: said.1,
    }
}

/// The fields of a `tool.result` layer's answer that it replaced; a read that
/// throws or a value that is no object replaced nothing.
fn rewritten<'js>(value: Value<'js>) -> (Option<String>, Option<bool>) {
    let Some(object) = value.as_object() else {
        return (None, None);
    };
    let text = object.get::<_, Option<String>>("text").ok().flatten();
    let is_error = object.get::<_, Option<bool>>("isError").ok().flatten();
    (text, is_error)
}

/// Runs every `turn.end` layer in load order and keeps the follow-ups.
fn chain_turn(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Arc<Watch>,
    notes: &Mutex<Notes>,
    index: u32,
    ran_tools: bool,
    text: &str,
) -> Vec<String> {
    let mut follow = Vec::new();
    for layer in &engine.turn {
        let Some(func) = restore(ctx, &layer.func) else {
            continue;
        };
        let out = watched(ctx, watch, notes, &layer.plugin, "hook turn.end", || {
            let payload = Object::new(ctx.clone()).map_err(|error| error.to_string())?;
            payload
                .set("index", index)
                .and_then(|_| payload.set("ranTools", ran_tools))
                .and_then(|_| payload.set("text", text))
                .map_err(|error| error.to_string())?;
            let value = call1(ctx, &func, payload.into_value())?;
            Ok(value)
        });
        match out {
            Ok(value) => {
                if let Some(next) = value.as_string().and_then(|text| text.to_string().ok())
                    && !next.is_empty()
                {
                    follow.push(next);
                }
            }
            Err(Failed::Threw(message)) => note(
                notes,
                format!("plugin {}: hook turn.end threw: {message}", layer.plugin),
            ),
            Err(Failed::Stopped) => {}
        }
    }
    follow
}

/// Calls every listener of one event; what they return is ignored.
fn fire(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Arc<Watch>,
    notes: &Mutex<Notes>,
    name: &str,
    payload: &JsonValue,
) {
    let what = format!("event {name}");
    for listener in engine
        .listeners
        .iter()
        .filter(|listener| listener.event == name)
    {
        let Some(func) = restore(ctx, &listener.func) else {
            continue;
        };
        let out = watched(ctx, watch, notes, &listener.plugin, &what, || {
            let value = api::from_json(ctx, payload).map_err(|error| error.to_string())?;
            call1(ctx, &func, value)?;
            Ok(())
        });
        if let Err(Failed::Threw(message)) = out {
            note(
                notes,
                format!("plugin {}: {what} threw: {message}", listener.plugin),
            );
        }
    }
}

/// Calls a JS function with one argument and settles its promise, if any.
fn call1<'js>(ctx: &Ctx<'js>, func: &Function<'js>, arg: Value<'js>) -> Result<Value<'js>, String> {
    match func.call::<_, Value>((arg,)) {
        Ok(value) => settle(ctx, value),
        Err(error) => Err(failure(ctx, error)),
    }
}

/// Calls a JS function with two arguments and settles its promise, if any.
fn call2<'js>(
    ctx: &Ctx<'js>,
    func: &Function<'js>,
    first: Value<'js>,
    second: Value<'js>,
) -> Result<Value<'js>, String> {
    match func.call::<_, Value>((first, second)) {
        Ok(value) => settle(ctx, value),
        Err(error) => Err(failure(ctx, error)),
    }
}

/// Drives the pending jobs of a value that may be a promise, and answers
/// with what it settled to.
fn settle<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Value<'js>, String> {
    match value.clone().into_promise() {
        Some(promise) => settle_promise(ctx, promise),
        None => Ok(value),
    }
}

/// Drives the pending jobs until `promise` settles; there is no timer or I/O
/// that could settle it later.
fn settle_promise<'js>(ctx: &Ctx<'js>, promise: Promise<'js>) -> Result<Value<'js>, String> {
    match promise.finish::<Value>() {
        Ok(value) => Ok(value),
        Err(Error::WouldBlock) => Err(NEVER.into()),
        Err(error) => Err(failure(ctx, error)),
    }
}

/// The message a QuickJS error carries, with the thrown value when it threw.
fn failure(ctx: &Ctx<'_>, error: Error) -> String {
    if error.is_exception() {
        api::thrown(ctx, ctx.catch())
    } else {
        error.to_string()
    }
}

/// Restores a saved function into this context.
fn restore<'js>(ctx: &Ctx<'js>, func: &Persistent<Function<'static>>) -> Option<Function<'js>> {
    func.clone().restore(ctx).ok()
}

/// The arguments text as the object a plugin sees; anything else is `{}`.
fn args_object(args: &str) -> JsonValue {
    match serde_json::from_str::<JsonValue>(args) {
        Ok(json) if json.is_object() => json,
        _ => JsonValue::Object(serde_json::Map::new()),
    }
}

/// Resolves and loads the imports of every plugin, inside its own folder.
#[derive(Clone)]
struct PluginLoader {
    /// The canonical folder of every plugin that loaded.
    folders: Arc<Mutex<HashSet<PathBuf>>>,
}

impl PluginLoader {
    /// The plugin folder `base` belongs to, the deepest one when they nest.
    fn folder_of(&self, base: &Path) -> Option<PathBuf> {
        let folders = self.folders.lock().ok()?;
        folders
            .iter()
            .filter(|folder| base.starts_with(folder))
            .max_by_key(|folder| folder.as_os_str().len())
            .cloned()
    }
}

impl Resolver for PluginLoader {
    fn resolve<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> JsResult<String> {
        // The importing module says which plugin's folder the import must
        // stay inside, never the plugin that loaded last.
        let base = Path::new(base);
        match self
            .folder_of(base)
            .and_then(|folder| resolve_import(&folder, base, name))
        {
            Some(path) => Ok(path.to_string_lossy().into_owned()),
            None => {
                let message = format!(
                    "cannot import '{name}': only relative paths inside the plugin folder load"
                );
                Err(Exception::throw_message(ctx, &message))
            }
        }
    }
}

impl Loader for PluginLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> JsResult<Module<'js, rquickjs::module::Declared>> {
        #[expect(
            clippy::disallowed_methods,
            reason = "runs on the plugin thread, off the async runtime"
        )]
        let source = std::fs::read_to_string(name)
            .map_err(|error| Error::new_loading_message(name, error.to_string()))?;
        Module::declare(ctx.clone(), name, source)
    }
}

/// The path `name` imports from `base`, when it stays inside `root`.
///
/// Both paths are canonical, so a symbol link out of the folder and a path
/// that walks out with `..` both fail the check; the import must name a file
/// that exists.
fn resolve_import(root: &Path, base: &Path, name: &str) -> Option<PathBuf> {
    if !name.starts_with('.') {
        // A bare specifier, and an absolute path, both leave the folder.
        return None;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread, off the async runtime"
    )]
    let path = std::fs::canonicalize(base.parent()?.join(name)).ok()?;
    path.starts_with(root).then_some(path)
}
