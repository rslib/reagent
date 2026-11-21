//! The `reagent` object a plugin's default export receives, and the staging
//! list its registrations land in until the plugin's load returned.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rquickjs::function::Opt;
use rquickjs::{Array, Ctx, Exception, Function, Object, Persistent, Value};
use serde_json::{Map, Value as JsonValue};

use super::runtime;
use super::{AnswerAsk, Answered, BY_BYTES, Live, Notes, SessionAsk, SessionOp, SessionReply};

/// The events a plugin may listen on.
///
/// `question_parked`, `question_forwarded`, `question_closed`,
/// `approval_parked` and `approval_closed` reach gateway plugins only: a normal
/// run never fires them.
const EVENTS: [&str; 13] = [
    "run_start",
    "turn_start",
    "turn_end",
    "tool_end",
    "run_end",
    "usage",
    "question_parked",
    "question_forwarded",
    "question_closed",
    "approval_parked",
    "approval_closed",
    "waiting",
    "waited",
];

/// What `reagent.gateway.answer` says of a wrong token or a wrong answer list.
const DENIED: &str = "denied";

/// What `reagent.gateway.answer` says of a question that is no longer open.
const STALE: &str = "stale";

/// The most bytes of a forward's note the journal keeps, and the model reads.
const NOTE_BYTES: usize = 1024;

/// The hooks a plugin may install.
pub(crate) const HOOKS: [&str; 3] = ["tool.call", "tool.result", "turn.end"];

/// The `timeoutMs` a `tool.call` layer has when it names none: the 5 s every
/// hook waited before a layer could name its own.
const TIMEOUT_MS: u64 = 5_000;

/// The longest `timeoutMs` a `tool.call` layer may name.
const TIMEOUT_MAX_MS: u64 = 120_000;

/// The budget a plugin tool's promise waits when the tool names no
/// `timeoutMs`: the two minutes every tool call waited before a tool could
/// name its own.
const TOOL_TIMEOUT_MS: u64 = 120_000;

/// The shortest and the longest `timeoutMs` a plugin tool may name: one second
/// and two hours.
const TOOL_TIMEOUT_MIN_MS: u64 = 1_000;
const TOOL_TIMEOUT_MAX_MS: u64 = 7_200_000;

/// The roles a panel line may carry; any other role is dropped.
const ROLES: [&str; 7] = ["text", "dim", "strong", "accent", "good", "warn", "bad"];

/// The most lines one panel keeps.
const LINES: usize = 50;

/// The longest text one panel line may carry, in bytes.
const LINE_BYTES: usize = 512;

/// The longest text `reagent.send` queues, in bytes.
const SENT_BYTES: usize = 32 * 1024;

/// The lines a panel holds before its first `set`.
const NO_LINES: &str = "[]";

/// The schema a tool gets when it registers without one.
const NO_PARAMETERS: &str = r#"{"type":"object","properties":{}}"#;

/// The bit of `"run_start"`, and so on; the low bit is the first event.
pub(crate) fn event_bit(name: &str) -> Option<u32> {
    EVENTS
        .iter()
        .position(|event| *event == name)
        .map(|at| 1 << at)
}

/// One tool a plugin registered while it loaded.
pub(crate) struct StagedTool {
    /// The name the model calls.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: JsonValue,
    /// How long its promise may wait: its own `timeoutMs`, or two minutes.
    pub timeout: Duration,
    /// The function that runs it.
    pub run: Persistent<Function<'static>>,
}

/// One command a plugin registered while it loaded.
pub(crate) struct StagedCommand {
    /// The name the user types after the slash.
    pub name: String,
    /// What the command does.
    pub description: String,
    /// The function that runs it.
    pub run: Persistent<Function<'static>>,
}

/// One `tool.call` layer a plugin installed while it loaded, with the limits it
/// named.
pub(crate) struct StagedCall {
    /// The function the layer runs.
    pub func: Persistent<Function<'static>>,
    /// How long the layer has to settle.
    pub timeout: Duration,
    /// What the call does when the layer does not settle in time, or throws.
    pub on_timeout: OnTimeout,
}

/// What a `tool.call` layer that does not settle in time, or throws, does to
/// the call.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OnTimeout {
    /// The call does not run, with a reason that names the plugin.
    Block,
    /// The call runs as it is, as a layer that failed always did.
    Keep,
}

/// One panel's state, shared by its live JS object and the load that commits
/// it.
pub(crate) struct Slot {
    /// The id clients see: `<plugin>/<id>`.
    pub id: String,
    /// What the panel is called.
    pub title: String,
    /// The lines, as one JSON array text.
    pub lines: String,
    /// Where a live `set` publishes, once the plugin's load returned.
    pub live: Option<Arc<Live>>,
}

/// What one plugin registered, kept aside until its load is known to have
/// returned without a throw.
#[derive(Default)]
pub(crate) struct Staging {
    /// The tools it registered.
    pub tools: Vec<StagedTool>,
    /// The commands it registered.
    pub commands: Vec<StagedCommand>,
    /// The panels it made, in creation order; the same list a second `panel`
    /// call finds an existing slot in.
    pub panels: Rc<RefCell<Vec<Rc<RefCell<Slot>>>>>,
    /// Its `tool.call` layers, in registration order.
    pub call: Vec<StagedCall>,
    /// Its `tool.result` layers, in registration order.
    pub result: Vec<Persistent<Function<'static>>>,
    /// Its `turn.end` layers, in registration order.
    pub turn: Vec<Persistent<Function<'static>>>,
    /// Its event listeners with the event each one waits on.
    pub listeners: Vec<(String, Persistent<Function<'static>>)>,
}

/// The host side a plugin's `reagent` object writes to besides its staging
/// list: the notes, the panels and sent texts, and the command names taken.
pub(crate) struct Host {
    /// The notes a refusal or a skipped line raises.
    pub notes: Arc<Mutex<Notes>>,
    /// The panels and the texts the client sees.
    pub live: Arc<Live>,
    /// Every command name another plugin took.
    pub taken: Rc<RefCell<HashSet<String>>>,
    /// Whether the plugin's load committed; a panel it makes later goes to
    /// `live` itself, because no load is left to commit it.
    pub committed: Rc<Cell<bool>>,
    /// The gateway's channel for `reagent.gateway.answer`, and `None` outside
    /// the gateway: every other plugin gets no `reagent.gateway` at all.
    pub gateway: Option<tokio::sync::mpsc::UnboundedSender<AnswerAsk>>,
    /// The hooks the plugin's manifest lets it install.
    pub hooks: Vec<String>,
    /// Whether the plugin's manifest grants `sessions`, which is what puts
    /// `reagent.sessions` on its object.
    pub sessions: bool,
    /// The run's channel a `reagent.sessions` call goes to, and `None` for a
    /// run with no host for the sessions it starts.
    pub talk: Option<tokio::sync::mpsc::UnboundedSender<SessionAsk>>,
    /// Whether this run may start no session of its own, which is a run a
    /// session started: every call rejects, and says why.
    pub sealed: bool,
    /// The index of this plugin, which a note about one of its listeners names.
    pub index: usize,
    /// The listeners of the sessions this plugin started, over the list the
    /// instance fires them from.
    pub watched: Rc<RefCell<Watched>>,
}

/// Builds the `reagent` object for the plugin `name` at `version`, whose
/// registrations land in `staging`.
///
/// `names` holds every tool name already taken, so a duplicate throws at
/// register time rather than when the plugin takes it.
pub(crate) fn reagent<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    version: &str,
    staging: &Rc<RefCell<Staging>>,
    names: &Rc<RefCell<HashSet<String>>>,
    host: &Host,
) -> rquickjs::Result<Value<'js>> {
    let reagent = Object::new(ctx.clone())?;
    let register = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        let names = Rc::clone(names);
        move |ctx: Ctx<'js>, spec: Object<'js>| -> rquickjs::Result<()> {
            let name: String = spec
                .get("name")
                .map_err(|_| fail(&ctx, "registerTool needs a string name"))?;
            if !named(&name) {
                return Err(fail(
                    &ctx,
                    format!("bad tool name '{name}': use 1 to 64 letters, digits, '_' or '-'"),
                ));
            }
            if names.borrow().contains(&name)
                || staging.borrow().tools.iter().any(|t| t.name == name)
            {
                return Err(fail(&ctx, format!("duplicate tool name '{name}'")));
            }
            let run: Function<'js> = spec
                .get("run")
                .map_err(|_| fail(&ctx, format!("tool '{name}' needs a run function")))?;
            let description = spec
                .get::<_, Option<String>>("description")
                .ok()
                .flatten()
                .unwrap_or_default();
            let parameters = match spec.get::<_, Option<Value>>("parameters") {
                Ok(Some(value)) if value.is_object() => json_of(&ctx, value)
                    .unwrap_or_else(|| serde_json::from_str(NO_PARAMETERS).unwrap_or_default()),
                Ok(Some(_)) => {
                    return Err(fail(
                        &ctx,
                        format!("tool '{name}' needs a JSON Schema object as its parameters"),
                    ));
                }
                _ => serde_json::from_str(NO_PARAMETERS).unwrap_or_default(),
            };
            let timeout = match spec.get::<_, Option<Value>>("timeoutMs") {
                Ok(Some(value)) => tool_timeout(&ctx, &name, &value)?,
                Ok(None) => Duration::from_millis(TOOL_TIMEOUT_MS),
                Err(_) => {
                    return Err(fail(
                        &ctx,
                        format!("tool '{name}': timeoutMs must be a whole number of milliseconds"),
                    ));
                }
            };
            staging.borrow_mut().tools.push(StagedTool {
                name,
                description,
                parameters,
                timeout,
                run: Persistent::save(&ctx, run),
            });
            Ok(())
        }
    })?;
    reagent.set("registerTool", register)?;
    let register_command = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        let taken = Rc::clone(&host.taken);
        move |ctx: Ctx<'js>, name: String, spec: Object<'js>| -> rquickjs::Result<()> {
            if !named(&name) {
                return Err(fail(
                    &ctx,
                    format!("bad command name '{name}': use 1 to 64 letters, digits, '_' or '-'"),
                ));
            }
            if taken.borrow().contains(&name)
                || staging.borrow().commands.iter().any(|c| c.name == name)
            {
                return Err(fail(&ctx, format!("duplicate command name '{name}'")));
            }
            let run: Function<'js> = spec
                .get("run")
                .map_err(|_| fail(&ctx, format!("command '{name}' needs a run function")))?;
            let description = spec
                .get::<_, Option<String>>("description")
                .ok()
                .flatten()
                .unwrap_or_default();
            staging.borrow_mut().commands.push(StagedCommand {
                name,
                description,
                run: Persistent::save(&ctx, run),
            });
            Ok(())
        }
    })?;
    reagent.set("registerCommand", register_command)?;
    let on = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        move |ctx: Ctx<'js>, event: String, listener: Function<'js>| -> rquickjs::Result<()> {
            if !EVENTS.contains(&event.as_str()) {
                return Err(fail(
                    &ctx,
                    format!("unknown event '{event}': use one of {}", EVENTS.join(", ")),
                ));
            }
            staging
                .borrow_mut()
                .listeners
                .push((event, Persistent::save(&ctx, listener)));
            Ok(())
        }
    })?;
    reagent.set("on", on)?;
    let hook = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        let hooks = Rc::new(host.hooks.clone());
        let plugin = name.to_string();
        move |ctx: Ctx<'js>,
              hook: String,
              layer: Function<'js>,
              options: Opt<Object<'js>>|
              -> rquickjs::Result<()> {
            let saved = Persistent::save(&ctx, layer);
            match hook.as_str() {
                "tool.call" => {
                    if !hooks.iter().any(|allowed| allowed == "tool.call") {
                        return Err(fail(
                            &ctx,
                            format!("plugin {plugin} has no hooks permission for tool.call"),
                        ));
                    }
                    let staged = call_layer(&ctx, saved, options.0)?;
                    staging.borrow_mut().call.push(staged);
                }
                "tool.result" => staging.borrow_mut().result.push(saved),
                "turn.end" => staging.borrow_mut().turn.push(saved),
                _ => {
                    return Err(fail(
                        &ctx,
                        format!("unknown hook '{hook}': use one of {}", HOOKS.join(", ")),
                    ));
                }
            }
            Ok(())
        }
    })?;
    reagent.set("hook", hook)?;
    let slots = Rc::clone(&staging.borrow().panels);
    let panel = Function::new(ctx.clone(), {
        let slots = Rc::clone(&slots);
        let notes = Arc::clone(&host.notes);
        let live = Arc::clone(&host.live);
        let committed = Rc::clone(&host.committed);
        let plugin = name.to_string();
        move |ctx: Ctx<'js>, spec: Object<'js>| -> rquickjs::Result<Value<'js>> {
            let id: String = spec
                .get("id")
                .map_err(|_| fail(&ctx, "ui.panel needs a string id"))?;
            if !named(&id) {
                return Err(fail(
                    &ctx,
                    format!("bad panel id '{id}': use 1 to 64 letters, digits, '_' or '-'"),
                ));
            }
            let title = spec
                .get::<_, Option<String>>("title")
                .ok()
                .flatten()
                .unwrap_or_default();
            let full = format!("{plugin}/{id}");
            let found = slots
                .borrow()
                .iter()
                .find(|slot| slot.borrow().id == full)
                .map(Rc::clone);
            let slot = match found {
                Some(found) => {
                    let mut slot = found.borrow_mut();
                    slot.title = title;
                    if let Some(live) = &slot.live {
                        live.retitle(&slot.id, &slot.title);
                    }
                    drop(slot);
                    found
                }
                None => {
                    let slot = Rc::new(RefCell::new(Slot {
                        id: full,
                        title,
                        lines: NO_LINES.into(),
                        live: None,
                    }));
                    // A load that already returned has nothing left to commit:
                    // the host sees the panel now, and its `set` from now. A
                    // load that throws never gets here, and publishes nothing.
                    if committed.get() {
                        {
                            let fresh = slot.borrow();
                            live.add_panel(
                                fresh.id.clone(),
                                fresh.title.clone(),
                                fresh.lines.clone(),
                            );
                        }
                        slot.borrow_mut().live = Some(Arc::clone(&live));
                    }
                    slots.borrow_mut().push(Rc::clone(&slot));
                    slot
                }
            };
            let object = Object::new(ctx.clone())?;
            object.set("id", slot.borrow().id.as_str())?;
            let set = Function::new(ctx.clone(), {
                let slot = Rc::clone(&slot);
                let notes = Arc::clone(&notes);
                move |ctx: Ctx<'js>, lines: Value<'js>| -> rquickjs::Result<()> {
                    let Some(array) = lines.as_array() else {
                        return Err(fail(&ctx, "panel.set needs an array of lines"));
                    };
                    // Read every item first: a getter may call `set` again.
                    let (text, skipped) = render(&ctx, array);
                    let id = {
                        let mut slot = slot.borrow_mut();
                        slot.lines = text;
                        if let Some(live) = &slot.live {
                            live.publish(&slot.id, &slot.lines);
                        }
                        slot.id.clone()
                    };
                    if skipped > 0 {
                        runtime::note(
                            &notes,
                            format!("plugins: panel {id} skipped {skipped} bad lines"),
                        );
                    }
                    Ok(())
                }
            })?;
            object.set("set", set)?;
            Ok(object.into_value())
        }
    })?;
    let ui = Object::new(ctx.clone())?;
    ui.set("panel", panel)?;
    reagent.set("ui", ui)?;
    let send = Function::new(ctx.clone(), {
        let live = Arc::clone(&host.live);
        move |text: String| {
            // An empty text is nothing to say.
            if !text.is_empty() {
                live.send(cut(&text, SENT_BYTES).to_owned());
            }
        }
    })?;
    reagent.set("send", send)?;
    let plugin = Object::new(ctx.clone())?;
    plugin.set("name", name)?;
    plugin.set("version", version)?;
    reagent.set("plugin", plugin)?;
    if let Some(answers) = &host.gateway {
        reagent.set("gateway", gateway(ctx, name, answers)?)?;
    }
    if host.sessions {
        reagent.set("sessions", sessions(ctx, name, host)?)?;
    }
    Ok(reagent.into_value())
}

/// The `reagent.gateway` object a gateway plugin gets, and no other plugin.
///
/// `answer(qid, token, answers, by)`, `approve(qid, token, answer, by,
/// reason)` and `forward(qid, token, to, note)` hand the work to the gateway's
/// async side and answer a Promise of `"ok"`, `"denied"`, `"stale"` or
/// `"busy"`, which settles when the gateway is done with the journal, the
/// parked folder and the worker it restarts. The plugin thread never waits on
/// any of that itself.
fn gateway<'js>(
    ctx: &Ctx<'js>,
    plugin: &str,
    answers: &tokio::sync::mpsc::UnboundedSender<AnswerAsk>,
) -> rquickjs::Result<Object<'js>> {
    let plugin = plugin.to_string();
    // Its name is part of the `by` the journal records: `gateway:<plugin>:<by>`.
    let answer = Function::new(ctx.clone(), {
        let answers = answers.clone();
        let plugin = plugin.clone();
        move |ctx: Ctx<'js>,
              qid: Value<'js>,
              token: Value<'js>,
              list: Value<'js>,
              by: Value<'js>|
              -> rquickjs::Result<Value<'js>> {
            let (promise, resolve, _reject) = ctx.promise()?;
            let refused = |verdict: &str| {
                let _ = resolve.call::<_, ()>((verdict,));
                promise.clone().into_value()
            };
            let (Some(qid), Some(token)) = (text_of(&qid), text_of(&token)) else {
                return Ok(refused(DENIED));
            };
            let list = match json_of(&ctx, list) {
                Some(list) if list.is_array() || list.is_null() => list.to_string(),
                _ => return Ok(refused(DENIED)),
            };
            let by = text_of(&by).unwrap_or_default();
            let (reply, done) = tokio::sync::oneshot::channel();
            let ask = AnswerAsk {
                plugin: plugin.clone(),
                qid,
                token,
                answered: Answered::Answers(list),
                by: cut(&by, BY_BYTES).to_owned(),
                reply,
            };
            Ok(hand(&ctx, &answers, ask, promise, resolve, done))
        }
    })?;
    let approve = Function::new(ctx.clone(), {
        let answers = answers.clone();
        let plugin = plugin.clone();
        move |ctx: Ctx<'js>,
              qid: Value<'js>,
              token: Value<'js>,
              word: Value<'js>,
              by: Value<'js>,
              reason: Opt<Value<'js>>|
              -> rquickjs::Result<Value<'js>> {
            let (promise, resolve, _reject) = ctx.promise()?;
            let (Some(qid), Some(token), Some(word)) =
                (text_of(&qid), text_of(&token), text_of(&word))
            else {
                let _ = resolve.call::<_, ()>((DENIED,));
                return Ok(promise.into_value());
            };
            let by = text_of(&by).unwrap_or_default();
            let reason = reason
                .0
                .filter(|reason| !reason.is_undefined() && !reason.is_null())
                .map(|reason| text_of(&reason));
            // A reason that is not text is a wrong answer, not one without a
            // reason.
            let reason = match reason {
                None => None,
                Some(Some(reason)) => Some(reason),
                Some(None) => {
                    let _ = resolve.call::<_, ()>((DENIED,));
                    return Ok(promise.into_value());
                }
            };
            let (reply, done) = tokio::sync::oneshot::channel();
            let ask = AnswerAsk {
                plugin: plugin.clone(),
                qid,
                token,
                answered: Answered::Approval { word, reason },
                by: cut(&by, BY_BYTES).to_owned(),
                reply,
            };
            Ok(hand(&ctx, &answers, ask, promise, resolve, done))
        }
    })?;
    let forward = Function::new(ctx.clone(), {
        let answers = answers.clone();
        move |ctx: Ctx<'js>,
              qid: Value<'js>,
              token: Value<'js>,
              to: Value<'js>,
              note: Opt<Value<'js>>|
              -> rquickjs::Result<Value<'js>> {
            let (promise, resolve, _reject) = ctx.promise()?;
            let (Some(qid), Some(token), Some(to)) = (text_of(&qid), text_of(&token), text_of(&to))
            else {
                let _ = resolve.call::<_, ()>((DENIED,));
                return Ok(promise.into_value());
            };
            let note = match note
                .0
                .filter(|note| !note.is_undefined() && !note.is_null())
            {
                None => String::new(),
                Some(note) => match text_of(&note) {
                    Some(note) => note,
                    None => {
                        let _ = resolve.call::<_, ()>((DENIED,));
                        return Ok(promise.into_value());
                    }
                },
            };
            let (reply, done) = tokio::sync::oneshot::channel();
            let ask = AnswerAsk {
                plugin: plugin.clone(),
                qid,
                token,
                answered: Answered::Forward {
                    to,
                    note: cut(&note, NOTE_BYTES).to_owned(),
                },
                by: String::new(),
                reply,
            };
            Ok(hand(&ctx, &answers, ask, promise, resolve, done))
        }
    })?;
    let object = Object::new(ctx.clone())?;
    object.set("answer", answer)?;
    object.set("approve", approve)?;
    object.set("forward", forward)?;
    Ok(object)
}

/// Hands one answer to the gateway, and settles `promise` with its verdict.
fn hand<'js>(
    ctx: &Ctx<'js>,
    answers: &tokio::sync::mpsc::UnboundedSender<AnswerAsk>,
    ask: AnswerAsk,
    promise: rquickjs::Promise<'js>,
    resolve: Function<'js>,
    done: tokio::sync::oneshot::Receiver<String>,
) -> Value<'js> {
    if answers.send(ask).is_err() {
        let _ = resolve.call::<_, ()>((STALE,));
        return promise.into_value();
    }
    // The gateway answers on its own thread; this future settles the promise
    // with what it decided, whatever the plugin awaits.
    ctx.spawn(async move {
        let verdict = match done.await {
            Ok(verdict) => verdict,
            Err(_) => STALE.to_string(),
        };
        let _ = resolve.call::<_, ()>((verdict,));
    });
    promise.into_value()
}

/// What a `reagent.sessions` call answers when the run has no host for the
/// sessions a plugin starts.
///
/// A build without the terminal UI starts no worker, so such a run has no host
/// and every call reads this.
const NO_HOST: &str = "this build has no sessions";

/// What a `reagent.sessions` call answers in a run a session started.
///
/// Such a run loads the same plugins as the run that started it, guard hooks
/// included, and starts none of its own: a session that started sessions would
/// have no depth limit.
const SEALED: &str = "a session cannot start sessions";

/// What a `reagent.sessions` call of this plugin's run answers when the run
/// starts no session.
fn refusal(host: &Host) -> &'static str {
    if host.sealed { SEALED } else { NO_HOST }
}

/// One listener of one of the sessions a plugin started.
pub(crate) struct SessionListener {
    /// The session it waits on.
    pub id: String,
    /// The number `on` answered, which removes it.
    pub at: u64,
    /// The plugin that added it, which a note names.
    pub plugin: String,
    /// The index of that plugin, which is what a stop of its children matches.
    pub index: usize,
    /// The function it runs.
    pub func: Persistent<Function<'static>>,
}

/// The listeners of the sessions one plugin started, in the order `on` added
/// them.
#[derive(Default)]
pub(crate) struct Watched {
    /// The listeners, oldest first.
    pub listeners: Vec<SessionListener>,
    /// The number the next listener takes.
    pub next: u64,
}

/// The `reagent.sessions` object a plugin whose manifest grants `sessions`
/// gets, and no other plugin.
///
/// Every call hands a [`SessionAsk`] to the run's async side and answers a
/// Promise of its outcome, which settles when that side has started, joined or
/// read the worker of the session. The plugin thread never waits on a worker.
fn sessions<'js>(ctx: &Ctx<'js>, plugin: &str, host: &Host) -> rquickjs::Result<Object<'js>> {
    let plugin = Rc::new(plugin.to_string());
    let talk = host.talk.clone();
    let sealed = refusal(host);
    let object = Object::new(ctx.clone())?;
    let start = Function::new(ctx.clone(), {
        let plugin = Rc::clone(&plugin);
        let talk = talk.clone();
        move |ctx: Ctx<'js>, spec: Opt<Object<'js>>| -> rquickjs::Result<Value<'js>> {
            let call = Call::new(&ctx)?;
            let Some(spec) = spec.0 else {
                return Ok(call.refuse(&ctx, "start needs { task }"));
            };
            let Some(task) = spec.get::<_, Option<String>>("task").ok().flatten() else {
                return Ok(call.refuse(&ctx, "start needs a string task"));
            };
            if task.trim().is_empty() {
                return Ok(call.refuse(&ctx, "start needs a task that is not empty"));
            }
            // A task of `-` is the word that reads a message from stdin, and a
            // worker's stdin is closed: it would wait for ever.
            if task.trim() == "-" {
                return Ok(call.refuse(
                    &ctx,
                    "start needs a task, and `-` would read a message from a stdin that is closed",
                ));
            }
            let cwd = spec
                .get::<_, Option<String>>("cwd")
                .ok()
                .flatten()
                .map(PathBuf::from);
            let model = spec.get::<_, Option<String>>("model").ok().flatten();
            let profile = spec.get::<_, Option<String>>("profile").ok().flatten();
            let provider = spec.get::<_, Option<String>>("provider").ok().flatten();
            // The tools of the child replace the ones it inherits, so a list
            // that is not a list would leave it without tools it cannot get
            // back.
            let tools = match spec.get::<_, Option<Value>>("tools") {
                Ok(None) => None,
                Ok(Some(value)) => match text_of(&value) {
                    Some(text) if !text.trim().is_empty() => Some(text),
                    _ => return Ok(call.refuse(&ctx, "tools must be a non-empty string")),
                },
                Err(_) => return Ok(call.refuse(&ctx, "tools must be a non-empty string")),
            };
            Ok(ask(
                &ctx,
                talk.as_ref(),
                sealed,
                &plugin,
                SessionOp::Start {
                    task,
                    cwd,
                    model,
                    profile,
                    provider,
                    tools,
                },
                call,
            ))
        }
    })?;
    object.set("start", start)?;
    let send = Function::new(ctx.clone(), {
        let plugin = Rc::clone(&plugin);
        let talk = talk.clone();
        move |ctx: Ctx<'js>, id: Value<'js>, text: Value<'js>| -> rquickjs::Result<Value<'js>> {
            let call = Call::new(&ctx)?;
            let (Some(id), Some(text)) = (text_of(&id), text_of(&text)) else {
                return Ok(call.refuse(&ctx, "send needs a session id and a text"));
            };
            Ok(ask(
                &ctx,
                talk.as_ref(),
                sealed,
                &plugin,
                SessionOp::Send { id, text },
                call,
            ))
        }
    })?;
    object.set("send", send)?;
    let steer = Function::new(ctx.clone(), {
        let plugin = Rc::clone(&plugin);
        let talk = talk.clone();
        move |ctx: Ctx<'js>, id: Value<'js>, text: Value<'js>| -> rquickjs::Result<Value<'js>> {
            let call = Call::new(&ctx)?;
            let (Some(id), Some(text)) = (text_of(&id), text_of(&text)) else {
                return Ok(call.refuse(&ctx, "steer needs a session id and a text"));
            };
            Ok(ask(
                &ctx,
                talk.as_ref(),
                sealed,
                &plugin,
                SessionOp::Steer { id, text },
                call,
            ))
        }
    })?;
    object.set("steer", steer)?;
    let cancel = Function::new(ctx.clone(), {
        let plugin = Rc::clone(&plugin);
        let talk = talk.clone();
        move |ctx: Ctx<'js>, id: Value<'js>| -> rquickjs::Result<Value<'js>> {
            let call = Call::new(&ctx)?;
            let Some(id) = text_of(&id) else {
                return Ok(call.refuse(&ctx, "cancel needs a session id"));
            };
            Ok(ask(
                &ctx,
                talk.as_ref(),
                sealed,
                &plugin,
                SessionOp::Cancel { id },
                call,
            ))
        }
    })?;
    object.set("cancel", cancel)?;
    let result = Function::new(ctx.clone(), {
        let plugin = Rc::clone(&plugin);
        let talk = talk.clone();
        move |ctx: Ctx<'js>, id: Value<'js>| -> rquickjs::Result<Value<'js>> {
            let call = Call::new(&ctx)?;
            let Some(id) = text_of(&id) else {
                return Ok(call.refuse(&ctx, "result needs a session id"));
            };
            Ok(ask(
                &ctx,
                talk.as_ref(),
                sealed,
                &plugin,
                SessionOp::Result { id },
                call,
            ))
        }
    })?;
    object.set("result", result)?;
    let list = Function::new(ctx.clone(), {
        let plugin = Rc::clone(&plugin);
        let talk = talk.clone();
        move |ctx: Ctx<'js>| -> rquickjs::Result<Value<'js>> {
            let call = Call::new(&ctx)?;
            Ok(ask(
                &ctx,
                talk.as_ref(),
                sealed,
                &plugin,
                SessionOp::List,
                call,
            ))
        }
    })?;
    object.set("list", list)?;
    let on = Function::new(ctx.clone(), {
        let plugin = Rc::clone(&plugin);
        let watched = Rc::clone(&host.watched);
        let index = host.index;
        move |ctx: Ctx<'js>, id: Value<'js>, func: Function<'js>| -> rquickjs::Result<Value<'js>> {
            let Some(id) = text_of(&id) else {
                return Err(fail(&ctx, "on needs a session id and a function"));
            };
            let at = {
                let mut list = watched.borrow_mut();
                let first = !list.listeners.iter().any(|entry| entry.id == id);
                let at = list.next;
                list.next += 1;
                list.listeners.push(SessionListener {
                    id: id.clone(),
                    at,
                    plugin: plugin.to_string(),
                    index,
                    func: Persistent::save(&ctx, func),
                });
                // The run forwards a session's frames only while the plugin has
                // a listener on it, so a session nobody watches costs nothing.
                if first {
                    forward(talk.as_ref(), &plugin, &id, true);
                }
                at
            };
            let remove = Function::new(ctx.clone(), {
                let talk = talk.clone();
                let plugin = Rc::clone(&plugin);
                let watched = Rc::clone(&watched);
                move || {
                    let mut list = watched.borrow_mut();
                    let Some(where_at) = list.listeners.iter().position(|entry| entry.at == at)
                    else {
                        return;
                    };
                    list.listeners.remove(where_at);
                    // The last listener is gone: the run stops forwarding.
                    if !list.listeners.iter().any(|entry| entry.id == id) {
                        forward(talk.as_ref(), &plugin, &id, false);
                    }
                }
            })?;
            Ok(remove.into_value())
        }
    })?;
    object.set("on", on)?;
    Ok(object)
}

/// Tells the run that the plugin wants the frames of session `id`, or no longer
/// wants them.
fn forward(
    talk: Option<&tokio::sync::mpsc::UnboundedSender<SessionAsk>>,
    plugin: &str,
    id: &str,
    want: bool,
) {
    let Some(talk) = talk else {
        return;
    };
    let (reply, _) = tokio::sync::oneshot::channel();
    let _ = talk.send(SessionAsk {
        plugin: plugin.to_string(),
        op: SessionOp::Watch {
            id: id.to_string(),
            forward: want,
        },
        reply,
    });
}

/// One `reagent.sessions` call in flight: the Promise the plugin awaits, and
/// the two ways it settles.
struct Call<'js> {
    /// What the plugin awaits.
    promise: rquickjs::Promise<'js>,
    /// Settles it with the answer.
    resolve: Function<'js>,
    /// Settles it with the reason the call did not happen.
    reject: Function<'js>,
}

impl<'js> Call<'js> {
    /// The unresolved Promise of one call.
    fn new(ctx: &Ctx<'js>) -> rquickjs::Result<Call<'js>> {
        let (promise, resolve, reject) = ctx.promise()?;
        Ok(Call {
            promise,
            resolve,
            reject,
        })
    }

    /// The Promise the plugin gets back.
    fn value(&self) -> Value<'js> {
        self.promise.clone().into_value()
    }

    /// Rejects the call with `reason`, and answers its Promise.
    fn refuse(&self, ctx: &Ctx<'js>, reason: &str) -> Value<'js> {
        if let Ok(error) = Exception::from_message(ctx.clone(), reason) {
            let _ = self.reject.call::<_, ()>((error,));
        }
        self.value()
    }

    /// Settles the call with what the run answered.
    fn settle(&self, ctx: &Ctx<'js>, answer: SessionReply) {
        let value = match answer {
            SessionReply::Started(id) => {
                let _ = self.resolve.call::<_, ()>((id,));
                return;
            }
            SessionReply::Done => {
                let _ = self.resolve.call::<_, ()>(());
                return;
            }
            SessionReply::List(ids) => from_json(ctx, &JsonValue::from(ids)),
            SessionReply::Ended { reason, text } => from_json(
                ctx,
                &JsonValue::Object(Map::from_iter([
                    ("reason".to_string(), JsonValue::from(reason)),
                    ("text".to_string(), JsonValue::from(text)),
                ])),
            ),
            SessionReply::Failed(reason) => {
                self.refuse(ctx, reason.as_str());
                return;
            }
        };
        if let Ok(value) = value {
            let _ = self.resolve.call::<_, ()>((value,));
        }
    }
}

/// Hands `op` to the run's session host and answers the Promise it settles.
///
/// The plugin thread never waits: the future that settles the Promise runs on
/// this plugin's own runtime, like a timer.
fn ask<'js>(
    ctx: &Ctx<'js>,
    talk: Option<&tokio::sync::mpsc::UnboundedSender<SessionAsk>>,
    refusal: &'static str,
    plugin: &str,
    op: SessionOp,
    call: Call<'js>,
) -> Value<'js> {
    let Some(talk) = talk else {
        return call.refuse(ctx, refusal);
    };
    let (reply, done) = tokio::sync::oneshot::channel();
    let sent = talk.send(SessionAsk {
        plugin: plugin.to_string(),
        op,
        reply,
    });
    if sent.is_err() {
        return call.refuse(ctx, refusal);
    }
    let value = call.value();
    // The plugin thread never waits: the Promise settles on this plugin's own
    // runtime, the way a timer's callback does.
    let inside = ctx.clone();
    ctx.spawn(async move {
        match done.await {
            Ok(answer) => call.settle(&inside, answer),
            Err(_) => {
                call.refuse(&inside, refusal);
            }
        }
    });
    value
}

/// A JS value as a string, when it is one.
fn text_of(value: &Value<'_>) -> Option<String> {
    value.as_string().and_then(|text| text.to_string().ok())
}

/// Throws `message` and returns the error that carries it.
fn fail(ctx: &Ctx<'_>, message: impl AsRef<str>) -> rquickjs::Error {
    Exception::throw_message(ctx, message.as_ref())
}

/// The `timeoutMs` a tool names, which is how long its promise may wait: a
/// whole number of milliseconds from 1000 to 7200000.
fn tool_timeout(ctx: &Ctx<'_>, tool: &str, value: &Value<'_>) -> rquickjs::Result<Duration> {
    let Some(ms) = value.as_int() else {
        return Err(fail(
            ctx,
            format!("tool '{tool}': timeoutMs must be a whole number of milliseconds"),
        ));
    };
    let Some(ms) = u64::try_from(ms)
        .ok()
        .filter(|ms| (TOOL_TIMEOUT_MIN_MS..=TOOL_TIMEOUT_MAX_MS).contains(ms))
    else {
        return Err(fail(
            ctx,
            format!(
                "tool '{tool}': timeoutMs must be {TOOL_TIMEOUT_MIN_MS} to {TOOL_TIMEOUT_MAX_MS} ms, not {ms}"
            ),
        ));
    };
    Ok(Duration::from_millis(ms))
}

/// The `onCancel` a tool call's context carries: the function the runtime runs
/// once when the run cancels that call. A later call replaces the earlier one.
pub(crate) type CancelHandler = Rc<RefCell<Option<Persistent<Function<'static>>>>>;

/// The `onCancel` function of one tool call, which keeps the function the
/// plugin hands it in `handler`.
pub(crate) fn on_cancel<'js>(
    ctx: &Ctx<'js>,
    handler: CancelHandler,
) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, func: Value<'js>| -> rquickjs::Result<()> {
            let Some(func) = func.as_function() else {
                return Err(fail(&ctx, "onCancel needs a function"));
            };
            *handler.borrow_mut() = Some(Persistent::save(&ctx, func.clone()));
            Ok(())
        },
    )
}

/// Whether `name` matches `^[a-zA-Z0-9_-]{1,64}$`.
fn named(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The `tool.call` layer `options` describes, with the limit and the outcome it
/// names: `timeoutMs` 1 to 120000, and `onTimeout` `"block"` or `"keep"`.
fn call_layer<'js>(
    ctx: &Ctx<'js>,
    func: Persistent<Function<'static>>,
    options: Option<Object<'js>>,
) -> rquickjs::Result<StagedCall> {
    let mut layer = StagedCall {
        func,
        timeout: Duration::from_millis(TIMEOUT_MS),
        on_timeout: OnTimeout::Keep,
    };
    let Some(options) = options else {
        return Ok(layer);
    };
    // A misspelt key would leave a gate on the defaults without a word, so
    // every key must be one this reads.
    for key in options.keys::<String>() {
        let key = key?;
        if key != "timeoutMs" && key != "onTimeout" {
            return Err(fail(
                ctx,
                format!("unknown hook option '{key}': use timeoutMs or onTimeout"),
            ));
        }
    }
    if let Some(value) = options.get::<_, Option<Value>>("timeoutMs")? {
        let Some(ms) = value.as_int() else {
            return Err(fail(
                ctx,
                "timeoutMs must be a whole number of milliseconds",
            ));
        };
        let Some(ms) = u64::try_from(ms)
            .ok()
            .filter(|ms| (1..=TIMEOUT_MAX_MS).contains(ms))
        else {
            return Err(fail(
                ctx,
                format!("timeoutMs must be 1 to {TIMEOUT_MAX_MS}, not {ms}"),
            ));
        };
        layer.timeout = Duration::from_millis(ms);
    }
    if let Some(value) = options.get::<_, Option<Value>>("onTimeout")? {
        let mode = value
            .as_string()
            .and_then(|text| text.to_string().ok())
            .unwrap_or_default();
        layer.on_timeout = match mode.as_str() {
            "block" => OnTimeout::Block,
            "keep" => OnTimeout::Keep,
            _ => return Err(fail(ctx, "onTimeout must be \"block\" or \"keep\"")),
        };
    }
    Ok(layer)
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a char
/// boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    &text[..text.floor_char_boundary(max)]
}

/// One panel's items as the JSON array text, and how many were skipped.
fn render<'js>(ctx: &Ctx<'js>, array: &Array<'js>) -> (String, usize) {
    let mut lines: Vec<JsonValue> = Vec::new();
    let mut skipped = 0;
    for at in 0..array.len().min(LINES) {
        let item = match array.get::<Value<'js>>(at) {
            Ok(item) => item,
            Err(_) => {
                // A throwing getter is one bad line, not a failed `set`.
                ctx.catch();
                skipped += 1;
                continue;
            }
        };
        match line(ctx, &item) {
            Some(line) => lines.push(line),
            None => skipped += 1,
        }
    }
    (JsonValue::Array(lines).to_string(), skipped)
}

/// One item of a `set` array as a line, or `None` when it is not one.
fn line<'js>(ctx: &Ctx<'js>, item: &Value<'js>) -> Option<JsonValue> {
    if let Some(text) = item.as_string().and_then(|text| text.to_string().ok()) {
        return Some(JsonValue::String(cut(&text, LINE_BYTES).to_owned()));
    }
    let object = item.as_object()?;
    if let Some(text) = field(ctx, object, "text") {
        let mut line = Map::new();
        line.insert(
            "text".into(),
            JsonValue::String(cut(&text, LINE_BYTES).to_owned()),
        );
        if let Some(role) = field(ctx, object, "role")
            && ROLES.contains(&role.as_str())
        {
            line.insert("role".into(), JsonValue::String(role));
        }
        return Some(JsonValue::Object(line));
    }
    if let Some(key) = field(ctx, object, "key") {
        let value = shown_value(ctx, object, "value")?;
        let mut line = Map::new();
        line.insert(
            "key".into(),
            JsonValue::String(cut(&key, LINE_BYTES).to_owned()),
        );
        line.insert("value".into(), JsonValue::String(value));
        return Some(JsonValue::Object(line));
    }
    if let Some(label) = field(ctx, object, "label") {
        let value = whole(ctx, object, "value")?;
        let max = whole(ctx, object, "max")?;
        let mut line = Map::new();
        line.insert(
            "label".into(),
            JsonValue::String(cut(&label, LINE_BYTES).to_owned()),
        );
        line.insert("value".into(), JsonValue::from(value));
        line.insert("max".into(), JsonValue::from(max));
        return Some(JsonValue::Object(line));
    }
    None
}

/// One string field of a line object; a read that throws is no field.
fn field<'js>(ctx: &Ctx<'js>, object: &Object<'js>, name: &str) -> Option<String> {
    match object.get::<_, Option<String>>(name) {
        Ok(value) => value,
        Err(_) => {
            ctx.catch();
            None
        }
    }
}

/// A `{ key, value }` value as the text to show: a number becomes its own.
fn shown_value<'js>(ctx: &Ctx<'js>, object: &Object<'js>, name: &str) -> Option<String> {
    let value = match object.get::<_, Option<Value<'js>>>(name) {
        Ok(value) => value?,
        Err(_) => {
            ctx.catch();
            return None;
        }
    };
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return Some(cut(&text, LINE_BYTES).to_owned());
    }
    if value.is_number() {
        return number_value(ctx, &value);
    }
    None
}

/// A `{ label, value, max }` number, clamped to a non-negative integer.
fn whole<'js>(ctx: &Ctx<'js>, object: &Object<'js>, name: &str) -> Option<u64> {
    let value = match object.get::<_, Option<Value<'js>>>(name) {
        Ok(value) => value?,
        Err(_) => {
            ctx.catch();
            return None;
        }
    };
    if !value.is_number() {
        return None;
    }
    let number: f64 = value.get().ok()?;
    // A fraction rounds down, `NaN` and everything below zero is 0, and
    // `Infinity` is the largest whole.
    Some(if number.is_nan() || number <= 0.0 {
        0
    } else if number >= u64::MAX as f64 {
        u64::MAX
    } else {
        number as u64
    })
}

/// A JSON number as the text of a `{ key, value }` line, spelled exactly as
/// the script's own `String(value)` spells it: `-0` is "0", `1e-7` stays
/// "1e-7" and `12` is "12".
fn number_value<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Option<String> {
    let to_text: Function = ctx.globals().get("String").ok()?;
    to_text.call::<_, String>((value.clone(),)).ok()
}

/// The JSON value of a JS value, as `JSON.stringify` writes it.
pub(crate) fn json_of<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Option<JsonValue> {
    let text = ctx.json_stringify(value).ok().flatten()?;
    serde_json::from_str(&text.to_string().ok()?).ok()
}

/// A JSON value as the JS value of the same shape.
pub(crate) fn from_json<'js>(ctx: &Ctx<'js>, value: &JsonValue) -> rquickjs::Result<Value<'js>> {
    use rquickjs::IntoJs;
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
                array.set(at, from_json(ctx, item)?)?;
            }
            array.into_value()
        }
        JsonValue::Object(fields) => {
            let object = Object::new(ctx.clone())?;
            for (name, field) in fields {
                object.set(name.as_str(), from_json(ctx, field)?)?;
            }
            object.into_value()
        }
    })
}

/// The message of the value a plugin threw.
pub(crate) fn thrown<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> String {
    if let Some(exception) = value.as_exception()
        && let Some(message) = exception.message()
    {
        return message;
    }
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return text;
    }
    json_of(ctx, value).map_or_else(
        || "the plugin threw a value".into(),
        |json| json.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_gate_every_event_has_a_bit_of_its_own() {
        let bits: Vec<u32> = EVENTS
            .iter()
            .map(|name| event_bit(name).expect("a bit"))
            .collect();
        let all = bits.iter().fold(0u32, |all, bit| all | bit);
        assert_eq!(all.count_ones() as usize, EVENTS.len(), "{bits:?}");
    }

    #[test]
    fn a_name_is_letters_digits_underscore_and_dash() {
        assert!(named("shout"));
        assert!(named("read_file-2"));
        assert!(!named(""));
        assert!(!named("has space"));
        assert!(!named(&"a".repeat(65)));
    }

    #[test]
    fn events_have_their_own_bit() {
        assert_eq!(event_bit("run_start"), Some(1));
        assert_eq!(event_bit("run_end"), Some(16));
        assert_eq!(event_bit("nothing"), None);
    }

    #[test]
    fn usage_event_has_its_own_bit() {
        assert_eq!(event_bit("usage"), Some(32));
    }

    #[test]
    fn a_cut_ends_on_a_char_boundary() {
        assert_eq!(cut("hello", 5), "hello");
        assert_eq!(cut("hello", 2), "he");
        assert_eq!(cut("a\u{e9}b", 2), "a");
        assert_eq!(cut("", 0), "");
    }
}
