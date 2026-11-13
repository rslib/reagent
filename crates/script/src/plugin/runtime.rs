//! The one thread every plugin runs on: a QuickJS runtime, its context, and
//! the persistent handles of what the plugins registered.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use rquickjs::function::{Opt, Rest, This};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::{
    AsyncContext, AsyncRuntime, Ctx, Error, Exception, Function, IntoJs, Module, Object,
    Persistent, Promise, Result as JsResult, Value,
};
use serde_json::Value as JsonValue;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use super::api::{self, Host, Slot, Staging};
use super::manifest::{self, Found, Manifest};
use super::{CommandInfo, Live, Notes};

/// The most bytes every plugin together may allocate.
const MEMORY_CAP: usize = 256 * 1024 * 1024;

/// How long one JS call may run before a note says it is slow.
const SLOW: Duration = Duration::from_secs(1);

/// How long one slice of JS may run before the runtime stops it: a slice is one
/// job or one callback, which the host runs between two polls of its own.
const STOP: Duration = Duration::from_secs(5);

/// How long one plugin tool or command may wait on the promise it handed back.
/// A hook, a listener and a timer callback wait [`STOP`] instead.
const LONG: Duration = Duration::from_secs(120);

/// What a plugin call that did not answer says when it was cut short, with the
/// budget that ended it.
fn stopped_after(limit: Duration) -> String {
    format!("the plugin call was stopped after {} s", limit.as_secs())
}

/// The most timers that may be waiting at once.
const TIMERS: usize = 1024;

/// The most timer futures that may be alive at once, counting the ones a
/// `clearTimeout` has ended but the scheduler has not dropped yet.
const RUNNING: usize = 16384;

/// The longest delay a timer may take, in milliseconds: about 24 days.
const LONGEST: f64 = 2_147_483_647.0;

/// One request the async side makes of the plugin thread.
pub(crate) enum Job {
    /// Loads every plugin under this root.
    Load {
        /// The folder to read.
        root: PathBuf,
        /// The folder a relative path of a plugin resolves against.
        session: PathBuf,
        /// The state folder a plugin's cookies are saved under.
        state: PathBuf,
        /// The sandbox helper every child process of a plugin starts through,
        /// and `None` when this run has no sandbox.
        sandbox: Option<PathBuf>,
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
    /// The watchdog stopped it, or a promise it handed back never settled: a
    /// note already says so, and this is the budget that ended it.
    Stopped(Duration),
}

/// Starts the plugin thread, with the notes the plugins raise.
pub(crate) fn start(notes: Arc<Mutex<Notes>>) -> Option<(Sender<Job>, Arc<Live>)> {
    let wake = match notes.lock() {
        Ok(notes) => notes.wake(),
        Err(broken) => broken.into_inner().wake(),
    };
    let live = Arc::new(Live::new(wake, Arc::clone(&notes)));
    let (jobs, queue) = tokio::sync::mpsc::channel(super::JOBS);
    let hosting = Arc::clone(&live);
    std::thread::Builder::new()
        .name("plugin".into())
        .spawn(move || host(queue, notes, hosting))
        .ok()?;
    Some((jobs, live))
}

/// The thread's whole life: one runtime, one context, one job at a time.
///
/// The thread has a tokio runtime of its own, so a plugin may hand back a
/// promise that a timer or a future settles: the host waits for it while the
/// runtime drives its own jobs.
fn host(queue: Receiver<Job>, notes: Arc<Mutex<Notes>>, live: Arc<Live>) {
    // I/O is what a plugin's `fetch` needs: its requests run on this runtime,
    // on this thread.
    let started = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build();
    let Ok(runtime) = started else {
        note(
            &notes,
            "plugins: the JavaScript runtime did not start".into(),
        );
        return;
    };
    // Local tasks run on this thread and are never sent anywhere else, which
    // is what a QuickJS runtime needs; `block_on` polls them while the host
    // waits for its next job.
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, serve(queue, notes, live));
}

/// The async host of the one runtime: sets it up, then answers every job.
///
/// Everything from the setup to the last job runs inside one `async_with`, the
/// rquickjs half of [`AsyncRuntime::drive`]: while the closure waits for the
/// next job, that future runs the pending promise jobs and polls the futures a
/// plugin spawned (its timers), and it holds the runtime lock only while it
/// polls. Driving the runtime as a second task instead would race this one for
/// that lock, so the pump of the one `async_with` is what drives it.
async fn serve(mut queue: Receiver<Job>, notes: Arc<Mutex<Notes>>, live: Arc<Live>) {
    let Ok(runtime) = AsyncRuntime::new() else {
        note(
            &notes,
            "plugins: the JavaScript runtime did not start".into(),
        );
        return;
    };
    runtime.set_memory_limit(MEMORY_CAP).await;
    let watch = Rc::new(Watch::new());
    runtime.set_interrupt_handler(Some(watch.handler())).await;
    let folders = Arc::new(Mutex::new(HashSet::new()));
    let loader = PluginLoader {
        folders: Arc::clone(&folders),
    };
    runtime.set_loader(loader.clone(), loader).await;
    let Ok(context) = AsyncContext::full(&runtime).await else {
        note(
            &notes,
            "plugins: the JavaScript context did not start".into(),
        );
        return;
    };
    context
        .async_with(async move |ctx| {
            // The timers of this context, emptied before it ends so no callback
            // outlives it.
            let clock = Rc::new(Timers::default());
            if let Err(error) = timers(&ctx, &clock, &watch, &notes) {
                note(
                    &notes,
                    format!("plugins: the timers did not start: {error}"),
                );
            }
            // The Node modules of this context, and the `process` object they
            // read, which this holds out of the globals; dropped here, before
            // the context it belongs to ends.
            #[cfg(feature = "node")]
            let held = match super::node::setup(&ctx).await {
                Ok(held) => Some(held),
                Err(error) => {
                    note(
                        &notes,
                        format!("plugins: the node modules did not start: {error}"),
                    );
                    None
                }
            };
            let mut engine = Engine::new();
            let mut waiting = Waiting {
                queue: &mut queue,
                watch: &watch,
                notes: &notes,
                ctx: &ctx,
                window: None,
            };
            while let Some(job) = (&mut waiting).await {
                handle(&ctx, &mut engine, &watch, &notes, &live, &folders, job).await;
            }
            clock.clear();
            #[cfg(feature = "node")]
            drop(held);
        })
        .await;
}

/// Answers one job.
async fn handle(
    ctx: &Ctx<'_>,
    engine: &mut Engine,
    watch: &Rc<Watch>,
    notes: &Arc<Mutex<Notes>>,
    live: &Arc<Live>,
    folders: &Mutex<HashSet<PathBuf>>,
    job: Job,
) {
    match job {
        Job::Load {
            root: dir,
            session,
            state,
            sandbox,
            reply,
        } => {
            super::node::session(&session);
            super::node::state(&state);
            super::node::sandbox(sandbox);
            let report = load_all(ctx, engine, &dir, watch, notes, live, folders).await;
            let _ = reply.send(report);
        }
        Job::Run { tool, args, reply } => {
            let out = run_tool(ctx, engine, watch, notes, &tool, &args).await;
            let _ = reply.send(out);
        }
        Job::Command { name, args, reply } => {
            let out = run_command(ctx, engine, watch, notes, &name, &args).await;
            let _ = reply.send(out);
        }
        Job::Call { tool, args, reply } => {
            let out = chain_call(ctx, engine, watch, notes, &tool, &args).await;
            let _ = reply.send(out);
        }
        Job::Result {
            tool,
            args,
            text,
            is_error,
            reply,
        } => {
            let out = chain_result(ctx, engine, watch, notes, &tool, &args, (text, is_error)).await;
            let _ = reply.send(out);
        }
        Job::Turn {
            index,
            ran_tools,
            text,
            reply,
        } => {
            let out = chain_turn(ctx, engine, watch, notes, index, ran_tools, &text).await;
            let _ = reply.send(out);
        }
        Job::Event { name, payload } => fire(ctx, engine, watch, notes, &name, &payload).await,
    }
}

/// The interrupt handler's clock: the windows of JS that run now.
///
/// A call may hand back a promise, a timer callback may run while it waits, and
/// the wait for the next job runs promise jobs: the windows overlap without
/// nesting last-in-first-out, so each one carries an id and ends by it, and the
/// handler watches the window whose deadline comes first.
struct Watch {
    /// When the runtime started, which every deadline counts from.
    start: Instant,
    /// The live windows, in the order they began.
    arms: RefCell<Vec<Arm>>,
    /// The id the last window took.
    last: Cell<u64>,
    /// When the slice of JS that runs now began: each job and each callback
    /// gets [`STOP`] of its own, so a call waiting on a promise may wait
    /// [`LONG`] without letting one job spin that long.
    slice: Cell<u64>,
}

/// One window of JS under the watchdog.
struct Arm {
    /// What ends the window.
    id: u64,
    /// When the window began, in nanoseconds after the runtime's start.
    began: u64,
    /// When a promise the window waits on may no longer settle, in the same
    /// unit.
    deadline: u64,
    /// How long the window may wait: [`STOP`] for a hook, a listener and a
    /// timer callback, [`LONG`] for a tool and a command.
    limit: Duration,
    /// Whether the window belongs to the wait for the next job, which has no
    /// call to name in a note.
    idle: bool,
    /// Whether the window passed [`SLOW`].
    slow: bool,
    /// The budget the handler stopped the window with.
    fired: Option<Duration>,
}

impl Watch {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            arms: RefCell::new(Vec::new()),
            last: Cell::new(0),
            slice: Cell::new(0),
        }
    }

    /// The handler QuickJS calls while it runs JS.
    fn handler(self: &Rc<Self>) -> Box<dyn FnMut() -> bool + 'static> {
        let watch = Rc::clone(self);
        Box::new(move || watch.check())
    }

    /// Notes every window past [`SLOW`], and stops the one whose deadline comes
    /// first, unless that is a window of the wait the runtime runs under.
    fn check(&self) -> bool {
        // No arm is borrowed while JS runs: `begin` and `end` take their borrow
        // on either side of the window they arm. A borrow that is somehow held
        // runs the window unarmed rather than panicking on this thread.
        let Ok(mut arms) = self.arms.try_borrow_mut() else {
            return false;
        };
        // The wait's window covers promise jobs no call asked for, and it notes
        // their stop itself: a call or a timer callback under way is what the
        // handler stops.
        let idle_only = arms.iter().all(|arm| arm.idle);
        let now = self.elapsed();
        let slow = SLOW.as_nanos() as u64;
        // The slice of JS that runs now stops at its own budget, whatever the
        // window it runs in may wait for.
        let slice = self.slice.get() + STOP.as_nanos() as u64;
        let mut first = None;
        let mut deadline = u64::MAX;
        let mut budget = STOP;
        for (at, arm) in arms.iter_mut().enumerate() {
            // Each window has its own clock: one that began before this window
            // is slow even while the JS of a later one runs.
            if now >= arm.began + slow {
                arm.slow = true;
            }
            if !idle_only && arm.idle {
                continue;
            }
            let (fire, limit) = if slice < arm.deadline {
                (slice, STOP)
            } else {
                (arm.deadline, arm.limit)
            };
            if fire < deadline {
                deadline = fire;
                budget = limit;
                first = Some(at);
            }
        }
        let Some(first) = first else {
            return false;
        };
        if now < deadline {
            return false;
        }
        arms[first].fired = Some(budget);
        true
    }

    /// Starts a new slice of JS: the runtime is about to run the jobs and the
    /// callbacks of one poll, and each of them gets [`STOP`] of its own.
    fn touch(&self) {
        self.slice.set(self.elapsed());
    }

    /// How long the runtime has run, in nanoseconds.
    fn elapsed(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// Arms the deadline of one call, hook or timer callback about to run, with
    /// the budget it may wait on its promise.
    fn begin(&self, limit: Duration) -> u64 {
        self.arm(limit, false)
    }

    /// Arms one window of the wait for the next job.
    fn begin_idle(&self) -> u64 {
        self.arm(STOP, true)
    }

    /// Arms one window, and answers with its id.
    fn arm(&self, limit: Duration, idle: bool) -> u64 {
        let began = self.elapsed();
        self.slice.set(began);
        let id = self.last.get().wrapping_add(1);
        self.last.set(id);
        self.arms.borrow_mut().push(Arm {
            id,
            began,
            deadline: began + limit.as_nanos() as u64,
            limit,
            idle,
            slow: false,
            fired: None,
        });
        id
    }

    /// Disarms the window with this id, wherever it sits: whether it was slow
    /// and which budget the handler stopped it with.
    fn end(&self, id: u64) -> (bool, Option<Duration>) {
        let mut arms = self.arms.borrow_mut();
        let Some(at) = arms.iter().position(|arm| arm.id == id) else {
            return (false, None);
        };
        let arm = arms.remove(at);
        (arm.slow, arm.fired)
    }

    /// The instant the window with this id is stopped at; [`STOP`] from now when
    /// it is no longer armed.
    ///
    /// A window's promise waits until that window's own deadline, never until
    /// the deadline of another window that happens to be open beside it.
    /// The budget of the window with this id: how long it may wait on the
    /// promise it handed back.
    fn limit_of(&self, id: u64) -> Duration {
        let arms = self.arms.borrow();
        arms.iter()
            .find(|arm| arm.id == id)
            .map_or(STOP, |arm| arm.limit)
    }

    fn deadline_of(&self, id: u64) -> tokio::time::Instant {
        let arms = self.arms.borrow();
        let first = arms
            .iter()
            .find(|arm| arm.id == id)
            .map(|arm| arm.deadline)
            .unwrap_or_else(|| self.elapsed() + STOP.as_nanos() as u64);
        tokio::time::Instant::from_std(self.start + Duration::from_nanos(first))
    }
}

/// The wait for the next job, with one window of JS armed for each poll.
///
/// While this waits, the runtime runs the promise jobs its plugins queued, and
/// the interrupt handler only fires within a window: the window of a poll is
/// what stops a job that never returns, and it ends on the next poll, so the JS
/// of one poll runs under exactly one deadline. A poll that ran nothing ends
/// its window without a note.
struct Waiting<'a, 'js> {
    /// The queue of jobs.
    queue: &'a mut Receiver<Job>,
    /// The watchdog the window belongs to.
    watch: &'a Watch,
    /// Where a stopped job leaves its note.
    notes: &'a Mutex<Notes>,
    /// The context, whose stopped job leaves an error to take out.
    ctx: &'a Ctx<'js>,
    /// The window of the last poll, if it is still armed.
    window: Option<u64>,
}

impl Future for Waiting<'_, '_> {
    type Output = Option<Job>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Job>> {
        let this = self.get_mut();
        // The jobs that ran since the last poll ran under that poll's window.
        this.end();
        this.watch.touch();
        this.window = Some(this.watch.begin_idle());
        if let Poll::Ready(next) = this.queue.poll_recv(cx) {
            // A job is about to run, and it arms a window of its own.
            this.end();
            return Poll::Ready(next);
        }
        Poll::Pending
    }
}

impl Waiting<'_, '_> {
    /// Ends the window of the last poll, and notes one it stopped.
    fn end(&mut self) {
        let Some(window) = self.window.take() else {
            return;
        };
        if self.watch.end(window).1.is_some() {
            // The interrupt left the error of the stopped job in the context.
            self.ctx.catch();
            note(
                self.notes,
                "plugins: a promise job stopped after 5 s".into(),
            );
        }
    }
}

/// The plugin a call is made for: the index a stop matches, and the name its
/// notes carry.
#[derive(Clone, Copy)]
struct Who<'a> {
    /// The index of the plugin, which is what a stop of its children matches.
    index: usize,
    /// The name of the plugin, which only a message uses.
    name: &'a str,
}

/// Runs one JS call under the watchdog, with `what` naming it in a note.
///
/// A call that hands back a promise is awaited here, so the window stays open
/// while the runtime settles it: a promise nothing settles is stopped after
/// `limit`, the same as a call that spins, which the slice budget stops after
/// [`STOP`]. The call gets its window's id, so what it awaits ends at this
/// window's deadline and no other's.
async fn watched<T>(
    ctx: &Ctx<'_>,
    watch: &Rc<Watch>,
    notes: &Mutex<Notes>,
    who: Who<'_>,
    what: &str,
    limit: Duration,
    call: impl AsyncFnOnce(u64) -> Result<T, Failed>,
) -> Result<T, Failed> {
    let since = Instant::now();
    let window = watch.begin(limit);
    let out = call(window).await;
    let (slow, fired) = watch.end(window);
    if fired.is_some() {
        // The interrupt left the thrown error in the context; take it out.
        ctx.catch();
    }
    let stopped = match (fired, &out) {
        (Some(budget), _) => Some(budget),
        (None, Err(Failed::Stopped(budget))) => Some(*budget),
        _ => None,
    };
    if let Some(budget) = stopped {
        // A call that was stopped leaves no child process behind: every child
        // its plugin started since it began goes at once. The index decides
        // which plugin that is, since a name may belong to another one too.
        super::node::stopped(who.index, since);
        note(
            notes,
            format!(
                "plugin {}: {what} stopped after {} s",
                who.name,
                budget.as_secs()
            ),
        );
        return Err(Failed::Stopped(budget));
    }
    if slow {
        note(notes, format!("plugin {}: {what} is slow", who.name));
    }
    out
}

/// Appends one note for [`super::Plugins::take_notes`].
pub(super) fn note(notes: &Mutex<Notes>, text: String) {
    if let Ok(mut notes) = notes.lock() {
        notes.push(text);
    }
}

/// Sets the timers of this context: reagent's own, since QuickJS has none.
///
/// `setTimeout` and `setInterval` hand back an id, and each waiting timer is one
/// future in the runtime's scheduler, so its callback runs while no job runs and
/// every callback runs under the watchdog. `queueMicrotask` queues a promise
/// job, so it runs before the next job the host takes.
fn timers<'js>(
    ctx: &Ctx<'js>,
    waiting: &Rc<Timers<'js>>,
    watch: &Rc<Watch>,
    notes: &Arc<Mutex<Notes>>,
) -> JsResult<()> {
    for repeat in [false, true] {
        let set = timer_setter(ctx, Rc::clone(waiting), watch, notes, repeat)?;
        let name = if repeat { "setInterval" } else { "setTimeout" };
        ctx.globals().set(name, set)?;
    }
    for name in ["clearTimeout", "clearInterval"] {
        let waiting = Rc::clone(waiting);
        let clear = Function::new(ctx.clone(), move |id: Opt<f64>| {
            if let Some(id) = id.0.and_then(id_of) {
                waiting.stop(id);
            }
        })?;
        ctx.globals().set(name, clear)?;
    }
    let queue = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, func: Value<'js>| -> JsResult<()> {
            let callback = function_of(&ctx, &func, "queueMicrotask")?;
            let (promise, resolve, _reject) = ctx.promise()?;
            promise
                .then()?
                .call::<_, ()>((This(promise.clone()), callback))?;
            resolve.call::<_, ()>(())
        },
    )?;
    ctx.globals().set("queueMicrotask", queue)?;
    // The count of live timer futures, for the test that a cleared timer ends
    // its future instead of holding it until the end of its delay.
    #[cfg(test)]
    {
        let waiting = Rc::clone(waiting);
        let count = Function::new(ctx.clone(), move || waiting.running.get() as f64)?;
        ctx.globals().set("__liveTimers", count)?;
    }
    Ok(())
}

/// The `setTimeout`, or `setInterval`, of one context.
fn timer_setter<'js>(
    ctx: &Ctx<'js>,
    waiting: Rc<Timers<'js>>,
    watch: &Rc<Watch>,
    notes: &Arc<Mutex<Notes>>,
    repeat: bool,
) -> JsResult<Function<'js>> {
    let watch = Rc::clone(watch);
    let notes = Arc::clone(notes);
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              func: Value<'js>,
              delay: Opt<f64>,
              args: Rest<Value<'js>>|
              -> JsResult<f64> {
            let name = if repeat { "setInterval" } else { "setTimeout" };
            let callback = function_of(&ctx, &func, name)?;
            let delay = delay_of(delay.0);
            let (cancel, mut cleared) = oneshot::channel();
            let timer = Timer {
                cancel,
                callback,
                args: args.0,
            };
            let Some(id) = waiting.add(timer) else {
                return Err(Exception::throw_range(&ctx, "too many timers are waiting"));
            };
            let waiting = Rc::clone(&waiting);
            let watch = Rc::clone(&watch);
            let notes = Arc::clone(&notes);
            // Counted while the future lives, however it ends, even if the
            // runtime drops it before its first poll.
            let alive = Alive(Rc::clone(&waiting));
            ctx.spawn(async move {
                let _alive = alive;
                loop {
                    // A timer cleared before it was polled ends here, without
                    // arming a sleep at all.
                    if !matches!(cleared.try_recv(), Err(TryRecvError::Empty)) {
                        return;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        _ = &mut cleared => return,
                    }
                    // The callback and its arguments wait in the map, so a
                    // `clearTimeout` frees them at once, even before the
                    // scheduler polls this future.
                    let Some((callback, args)) = waiting.next(id, repeat) else {
                        return;
                    };
                    tick(&watch, &notes, &callback, args).await;
                    if !repeat {
                        return;
                    }
                }
            });
            Ok(f64::from(id))
        },
    )
}

/// Runs one timer callback under the watchdog; a throw or a stop is one note.
///
/// An interval keeps running after either, so one bad run does not end it.
async fn tick<'js>(
    watch: &Watch,
    notes: &Mutex<Notes>,
    func: &Function<'js>,
    args: Vec<Value<'js>>,
) {
    let window = watch.begin(STOP);
    let ctx = func.ctx();
    let out = match func.call::<_, Value>((Rest(args),)) {
        Ok(value) => settle(ctx, watch, value, window).await,
        Err(error) => Err(failure(ctx, error)),
    };
    let (_, fired) = watch.end(window);
    if fired.is_some() {
        ctx.catch();
    }
    match out {
        Ok(_) => {}
        Err(Failed::Threw(reason)) if fired.is_none() => {
            note(notes, format!("plugins: a timer callback threw: {reason}"));
        }
        Err(_) => note(notes, "plugins: a timer callback stopped after 5 s".into()),
    }
}

/// The function a timer or a microtask names, or a `TypeError`.
fn function_of<'js>(ctx: &Ctx<'js>, value: &Value<'js>, name: &str) -> JsResult<Function<'js>> {
    match value.as_function() {
        Some(func) => Ok(func.clone()),
        None => Err(Exception::throw_type(
            ctx,
            &format!("{name} needs a function"),
        )),
    }
}

/// How long a timer waits: at least 1 ms, and no longer than [`LONGEST`].
fn delay_of(ms: Option<f64>) -> Duration {
    let ms = ms.filter(|ms| !ms.is_nan()).unwrap_or(0.0);
    Duration::from_secs_f64(ms.clamp(1.0, LONGEST) / 1000.0)
}

/// The timer id a `clearTimeout` names; anything else names no timer.
fn id_of(id: f64) -> Option<u32> {
    (id >= 1.0 && id <= f64::from(u32::MAX)).then_some(id as u32)
}

/// Every timer of the context, by id.
#[derive(Default)]
struct Timers<'js> {
    /// The id the last timer took.
    last: Cell<u32>,
    /// The timers waiting now, with what each one runs and the signal that ends
    /// its future.
    live: RefCell<HashMap<u32, Timer<'js>>>,
    /// How many timer futures are alive, ended ones included until the
    /// scheduler drops them.
    running: Cell<usize>,
}

/// One timer that is waiting: its callback and arguments, which live here so a
/// `clearTimeout` frees them at once, and the signal that ends its future.
struct Timer<'js> {
    /// The signal that ends the timer's future.
    cancel: oneshot::Sender<()>,
    /// The function the timer runs.
    callback: Function<'js>,
    /// The arguments it takes.
    args: Vec<Value<'js>>,
}

impl<'js> Timers<'js> {
    /// Takes an id for one more timer; `None` when too many are waiting or too
    /// many futures are still alive.
    fn add(&self, timer: Timer<'js>) -> Option<u32> {
        let mut waiting = self.live.borrow_mut();
        if waiting.len() >= TIMERS || self.running.get() >= RUNNING {
            return None;
        }
        let id = self.last.get().wrapping_add(1);
        self.last.set(id);
        waiting.insert(id, timer);
        self.running.set(self.running.get() + 1);
        Some(id)
    }

    /// Stops the timer with this id; an id that is not waiting is nothing. The
    /// callback and its arguments are freed here, before the future that waits
    /// on them is polled again.
    fn stop(&self, id: u32) {
        if let Some(timer) = self.live.borrow_mut().remove(&id) {
            // The future ends at once instead of at the end of a sleep.
            let _ = timer.cancel.send(());
        }
    }

    /// Takes the callback and arguments of the timer about to run; a repeating
    /// timer keeps its entry, so it stays waiting and can be cleared, and a
    /// one-shot's entry is spent.
    fn next(&self, id: u32, repeat: bool) -> Option<(Function<'js>, Vec<Value<'js>>)> {
        let mut waiting = self.live.borrow_mut();
        if repeat {
            let timer = waiting.get(&id)?;
            return Some((timer.callback.clone(), timer.args.clone()));
        }
        let timer = waiting.remove(&id)?;
        Some((timer.callback, timer.args))
    }

    /// Counts one timer future that is gone.
    fn finished(&self) {
        self.running.set(self.running.get().saturating_sub(1));
    }

    /// Drops every timer still waiting, so the callbacks and arguments it holds
    /// are freed while the context that owns them is still alive.
    fn clear(&self) {
        self.live.borrow_mut().clear();
    }
}

/// Keeps one timer future counted while it lives, however it ends.
struct Alive<'js>(Rc<Timers<'js>>);

impl Drop for Alive<'_> {
    fn drop(&mut self) {
        self.0.finished();
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
    /// The index of that plugin, which is what a stop matches.
    index: usize,
    /// The function that runs it.
    run: Persistent<Function<'static>>,
}

/// One command a plugin registered, with the plugin that owns it.
struct RegisteredCommand {
    /// The name the user types after the slash.
    name: String,
    /// The plugin that registered it.
    plugin: String,
    /// The index of that plugin, which is what a stop matches.
    index: usize,
    /// The function that runs it.
    run: Persistent<Function<'static>>,
}

/// One hook layer, with the plugin that installed it.
struct Layer {
    /// The plugin that installed it.
    plugin: String,
    /// The index of that plugin, which is what a stop matches.
    index: usize,
    /// The function the layer runs.
    func: Persistent<Function<'static>>,
}

/// One event listener, with the plugin that registered it.
struct Listener {
    /// The plugin that registered it.
    plugin: String,
    /// The index of that plugin, which is what a stop matches.
    index: usize,
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
async fn load_all(
    ctx: &Ctx<'_>,
    engine: &mut Engine,
    root: &Path,
    watch: &Rc<Watch>,
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
                match load_one(ctx, engine, &manifest, watch, notes, live, folders).await {
                    Ok(owned) => {
                        names.push((manifest.name.clone(), dir));
                        report.loaded += 1;
                        report.tools.extend(owned.tools);
                        report.commands.extend(owned.commands);
                    }
                    Err(Failed::Threw(reason)) => {
                        note(notes, format!("plugin {folder}: {reason}"));
                    }
                    Err(Failed::Stopped(_)) => {}
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
async fn load_one(
    ctx: &Ctx<'_>,
    engine: &mut Engine,
    manifest: &Manifest,
    watch: &Rc<Watch>,
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
    // the folder of the plugin that happened to load last. Its place is the
    // index its Node wrapper modules carry.
    let index = match folders.lock() {
        Ok(mut folders) => {
            let index = folders.len();
            folders.insert(manifest.dir.clone());
            index
        }
        Err(_) => 0,
    };
    match super::node::register(index, manifest) {
        Ok(lines) => {
            for line in lines {
                note(notes, line);
            }
        }
        Err(reason) => return Err(Failed::Threw(reason)),
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
    watched(
        ctx,
        watch,
        notes,
        Who {
            index,
            name: &manifest.name,
        },
        "load",
        STOP,
        async |window| {
            let reagent = api::reagent(
                ctx,
                &manifest.name,
                &manifest.version,
                &staging,
                &engine.names,
                &host,
            )
            .map_err(|error| failure(ctx, error))?;
            let module =
                Module::declare(ctx.clone(), name, source).map_err(|error| failure(ctx, error))?;
            let (module, promise) = module.eval().map_err(|error| failure(ctx, error))?;
            settle_promise(ctx, watch, promise, window).await?;
            let entry = module
                .get::<_, Option<Function>>("default")
                .map_err(|error| failure(ctx, error))?
                .ok_or_else(|| Failed::Threw("the entry has no default export".into()))?;
            call1(ctx, watch, window, &entry, reagent).await?;
            Ok(())
        },
    )
    .await?;
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
            index,
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
            index,
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
        index,
        func,
    };
    engine.call.extend(staged.call.into_iter().map(layer));
    engine.result.extend(staged.result.into_iter().map(layer));
    engine.turn.extend(staged.turn.into_iter().map(layer));
    engine
        .listeners
        .extend(staged.listeners.into_iter().map(|(event, func)| Listener {
            plugin: manifest.name.clone(),
            index,
            event,
            func,
        }));
    Ok(Owned { tools, commands })
}

/// Runs one slash command and turns whatever it returned into the text to
/// show.
async fn run_command(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Rc<Watch>,
    notes: &Mutex<Notes>,
    name: &str,
    args: &str,
) -> Option<String> {
    let Some(registered) = engine.commands.iter().find(|entry| entry.name == name) else {
        return Some(format!("plugins: no command is named '/{name}'"));
    };
    let plugin = registered.plugin.as_str();
    let Some(func) = restore(ctx, &registered.run) else {
        return Some(format!(
            "plugin {plugin}: command /{name}: {}",
            stopped_after(LONG)
        ));
    };
    let what = format!("command {name}");
    let out = watched(
        ctx,
        watch,
        notes,
        Who {
            index: registered.index,
            name: plugin,
        },
        &what,
        LONG,
        async |window| {
            let value = args.into_js(ctx).map_err(|error| failure(ctx, error))?;
            let value = call1(ctx, watch, window, &func, value).await?;
            shown(ctx, value)
        },
    )
    .await;
    match out {
        Ok(text) => text,
        Err(Failed::Threw(reason)) => {
            Some(format!("plugin {plugin}: command /{name} threw: {reason}"))
        }
        Err(Failed::Stopped(budget)) => Some(format!(
            "plugin {plugin}: command /{name} stopped after {} s",
            budget.as_secs()
        )),
    }
}

/// The text a command's return value stands for; no value shows nothing.
fn shown<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Option<String>, Failed> {
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
async fn run_tool(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Rc<Watch>,
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
        return stopped(LONG);
    };
    let parsed = args_object(args);
    let out = watched(
        ctx,
        watch,
        notes,
        Who {
            index: registered.index,
            name: &registered.plugin,
        },
        &format!("tool {tool}"),
        LONG,
        async |window| {
            let arguments = api::from_json(ctx, &parsed).map_err(|error| failure(ctx, error))?;
            let context = Object::new(ctx.clone()).map_err(|error| failure(ctx, error))?;
            context
                .set("tool", tool)
                .and_then(|_| context.set("plugin", registered.plugin.as_str()))
                .map_err(|error| failure(ctx, error))?;
            let value = call2(ctx, watch, window, &func, arguments, context.into_value()).await?;
            Ok(answered(ctx, value))
        },
    )
    .await;
    match out {
        Ok(reply) => reply,
        Err(Failed::Threw(text)) => RunReply {
            text,
            is_error: true,
        },
        Err(Failed::Stopped(budget)) => stopped(budget),
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

/// The result a call reports when the runtime could not run it, with the budget
/// that ended it.
fn stopped(limit: Duration) -> RunReply {
    RunReply {
        text: stopped_after(limit),
        is_error: true,
    }
}

/// Runs every `tool.call` layer in load order, each on the previous value.
async fn chain_call(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Rc<Watch>,
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
        let out = watched(
            ctx,
            watch,
            notes,
            Who {
                index: layer.index,
                name: &layer.plugin,
            },
            "hook tool.call",
            STOP,
            async |window| {
                let payload = Object::new(ctx.clone()).map_err(|error| failure(ctx, error))?;
                payload
                    .set("tool", tool)
                    .and_then(|_| payload.set("args", api::from_json(ctx, &current)?))
                    .map_err(|error| failure(ctx, error))?;
                let value = call1(ctx, watch, window, &func, payload.into_value()).await?;
                Ok(step(ctx, &layer.plugin, value))
            },
        )
        .await;
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
            Err(Failed::Stopped(_)) => {}
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
async fn chain_result(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Rc<Watch>,
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
        let out = watched(
            ctx,
            watch,
            notes,
            Who {
                index: layer.index,
                name: &layer.plugin,
            },
            "hook tool.result",
            STOP,
            async |window| {
                let payload = Object::new(ctx.clone()).map_err(|error| failure(ctx, error))?;
                payload
                    .set("tool", tool)
                    .and_then(|_| payload.set("args", api::from_json(ctx, &parsed)?))
                    .and_then(|_| payload.set("text", text.as_str()))
                    .and_then(|_| payload.set("isError", is_error))
                    .map_err(|error| failure(ctx, error))?;
                let value = call1(ctx, watch, window, &func, payload.into_value()).await?;
                // Reading the answer runs getters, so it stays under the watchdog.
                Ok(rewritten(value))
            },
        )
        .await;
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
            Err(Failed::Stopped(_)) => {}
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
async fn chain_turn(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Rc<Watch>,
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
        let out = watched(
            ctx,
            watch,
            notes,
            Who {
                index: layer.index,
                name: &layer.plugin,
            },
            "hook turn.end",
            STOP,
            async |window| {
                let payload = Object::new(ctx.clone()).map_err(|error| failure(ctx, error))?;
                payload
                    .set("index", index)
                    .and_then(|_| payload.set("ranTools", ran_tools))
                    .and_then(|_| payload.set("text", text))
                    .map_err(|error| failure(ctx, error))?;
                call1(ctx, watch, window, &func, payload.into_value()).await
            },
        )
        .await;
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
            Err(Failed::Stopped(_)) => {}
        }
    }
    follow
}

/// Calls every listener of one event; what they return is ignored.
async fn fire(
    ctx: &Ctx<'_>,
    engine: &Engine,
    watch: &Rc<Watch>,
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
        let out = watched(
            ctx,
            watch,
            notes,
            Who {
                index: listener.index,
                name: &listener.plugin,
            },
            &what,
            STOP,
            async |window| {
                let value = api::from_json(ctx, payload).map_err(|error| failure(ctx, error))?;
                call1(ctx, watch, window, &func, value).await?;
                Ok(())
            },
        )
        .await;
        if let Err(Failed::Threw(message)) = out {
            note(
                notes,
                format!("plugin {}: {what} threw: {message}", listener.plugin),
            );
        }
    }
}

/// Calls a JS function with one argument and awaits its promise, if any.
async fn call1<'js>(
    ctx: &Ctx<'js>,
    watch: &Watch,
    window: u64,
    func: &Function<'js>,
    arg: Value<'js>,
) -> Result<Value<'js>, Failed> {
    let value = func
        .call::<_, Value>((arg,))
        .map_err(|error| failure(ctx, error))?;
    settle(ctx, watch, value, window).await
}

/// Calls a JS function with two arguments and awaits its promise, if any.
async fn call2<'js>(
    ctx: &Ctx<'js>,
    watch: &Watch,
    window: u64,
    func: &Function<'js>,
    first: Value<'js>,
    second: Value<'js>,
) -> Result<Value<'js>, Failed> {
    let value = func
        .call::<_, Value>((first, second))
        .map_err(|error| failure(ctx, error))?;
    settle(ctx, watch, value, window).await
}

/// Awaits a value that may be a promise, and answers with what it settled to.
async fn settle<'js>(
    ctx: &Ctx<'js>,
    watch: &Watch,
    value: Value<'js>,
    window: u64,
) -> Result<Value<'js>, Failed> {
    match value.clone().into_promise() {
        Some(promise) => settle_promise(ctx, watch, promise, window).await,
        None => Ok(value),
    }
}

/// Awaits a promise while the runtime settles it, until the window armed for
/// this call reaches its budget: [`LONG`] for a tool or a command, [`STOP`] for
/// a hook, a listener or a timer callback.
///
/// The deadline is the call's own, and the JS that runs between two polls of the
/// promise is one slice, so a promise that no timer or future can settle is a
/// stopped call, the same as a call that spins: the caller names it in a note.
async fn settle_promise<'js>(
    ctx: &Ctx<'js>,
    watch: &Watch,
    promise: Promise<'js>,
    window: u64,
) -> Result<Value<'js>, Failed> {
    let deadline = watch.deadline_of(window);
    let awaited = Sliced {
        watch,
        inner: promise.into_future::<Value>(),
    };
    match tokio::time::timeout_at(deadline, awaited).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(failure(ctx, error)),
        Err(_) => Err(Failed::Stopped(watch.limit_of(window))),
    }
}

/// A future that starts a slice of JS at every poll: the runtime runs the jobs
/// that are ready right after a poll, so each batch of them gets [`STOP`] of its
/// own.
struct Sliced<'a, F> {
    /// The watchdog the slice belongs to.
    watch: &'a Watch,
    /// What is awaited.
    inner: F,
}

impl<F: Future + Unpin> Future for Sliced<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        this.watch.touch();
        Pin::new(&mut this.inner).poll(cx)
    }
}

/// The failure a QuickJS error stands for, with what it threw.
fn failure(ctx: &Ctx<'_>, error: Error) -> Failed {
    if error.is_exception() {
        Failed::Threw(api::thrown(ctx, ctx.catch()))
    } else {
        Failed::Threw(error.to_string())
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
        // A Node module name, and the modules of a Node wrapper, come first:
        // they name no file of any plugin folder.
        if let Some(found) = super::node::resolve(ctx, base, name) {
            return found;
        }
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
        // The wrapper of a plugin's Node module, and the raw module it wraps,
        // are declared here and read no file.
        #[cfg(feature = "node")]
        {
            if let Some(module) = super::node::load(ctx, name) {
                return module;
            }
        }
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
