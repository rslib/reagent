//! Plugins: a folder of ES modules that adds tools and hooks to a run.
//!
//! Every plugin under one root loads into a single QuickJS runtime on one
//! thread of its own; the agent sees only the tools they register and the
//! hooks they install. A plugin that fails to load is skipped with a note,
//! and a layer or listener that throws is skipped the same way, so one bad
//! plugin never takes the run down with it.

mod api;
#[cfg(feature = "node")]
pub mod install;
pub mod lock;
mod manifest;
mod node;
mod runtime;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use reagent_agent::hook::{CallHook, CallVerdict, TurnHook};
use reagent_agent::policy::TurnView;
use reagent_agent::tool::{Tool, ToolOutput};
use reagent_ai::{Content, Text, ToolSpec};
use serde_json::Value as JsonValue;
use tokio::sync::Notify;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use runtime::{CallReply, Job, RunReply, Served, ToolInfo};

/// The most times in a row a plugin may add a follow-up.
const FOLLOW_UPS: u32 = 3;

/// The longest a hook chain may take before its answer is treated as keep.
///
/// No `tool.call` chain has this cap: each of its layers names its own limit,
/// which is what bounds that chain.
const CHAIN_TIME: Duration = Duration::from_secs(30);

/// The most notes kept at once; the rest are counted, not stored.
const NOTES: usize = 100;

/// The longest `by` a gateway plugin may pass, in bytes.
const BY_BYTES: usize = 64;

/// One answer a gateway plugin asks the gateway to record, and where its
/// result goes.
///
/// The plugin thread hands this to the gateway's async side, which owns the
/// journal, the parked folder and the worker it restarts.
pub struct AnswerAsk {
    /// The plugin that asked, as its manifest names it.
    pub plugin: String,
    /// The question it answers.
    pub qid: String,
    /// The token the gateway handed it with `question_parked`.
    pub token: String,
    /// The answers, as JSON text, or `null` for none.
    pub answers: String,
    /// Who answered, in the plugin's own words, such as a chat user name.
    pub by: String,
    /// Carries `"ok"`, `"denied"`, `"stale"` or `"busy"` back to the plugin.
    pub reply: tokio::sync::oneshot::Sender<String>,
}

impl fmt::Debug for AnswerAsk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnswerAsk")
            .field("plugin", &self.plugin)
            .field("qid", &self.qid)
            .field("answers", &self.answers)
            .field("by", &self.by)
            .finish_non_exhaustive()
    }
}

/// One request a plugin's `reagent.sessions` hands to the run it belongs to.
///
/// The plugin thread hands this to the run's async side, which starts the
/// worker of a session and joins it as a client, so the plugin thread never
/// waits on a worker.
pub struct SessionAsk {
    /// The plugin that asked, as its manifest names it.
    pub plugin: String,
    /// What it asks for.
    pub op: SessionOp,
    /// Carries the outcome back to the plugin.
    pub reply: tokio::sync::oneshot::Sender<SessionReply>,
}

impl fmt::Debug for SessionAsk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionAsk")
            .field("plugin", &self.plugin)
            .field("op", &self.op)
            .finish_non_exhaustive()
    }
}

/// What a plugin's `reagent.sessions` may ask of its run.
#[derive(Debug)]
pub enum SessionOp {
    /// Start a session on a task.
    Start {
        /// The task, which is the first message of the session.
        task: String,
        /// The folder it works in, or `None` for the folder of the run.
        cwd: Option<PathBuf>,
        /// The model it starts on, as `--model` names it.
        model: Option<String>,
        /// The config profile it reads, as `--profile` names it.
        profile: Option<String>,
        /// The provider it starts on, as `--provider` names it, which is the
        /// provider of the run when it is `None`.
        provider: Option<String>,
    },
    /// Queue `text` as the next message of the session.
    Send {
        /// The session.
        id: String,
        /// The text to queue.
        text: String,
    },
    /// Steer the run of the session with `text`.
    Steer {
        /// The session.
        id: String,
        /// The text the running turn reads.
        text: String,
    },
    /// Stop the turn the session runs.
    Cancel {
        /// The session.
        id: String,
    },
    /// Answer when the run of the session ends.
    Result {
        /// The session.
        id: String,
    },
    /// Forward the frames of the session to the plugin's listeners, or stop.
    Watch {
        /// The session.
        id: String,
        /// Whether to forward them.
        forward: bool,
    },
    /// The sessions of the plugin that are still open.
    List,
}

/// What the run answers one `reagent.sessions` call with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionReply {
    /// The id of the session `start` made.
    Started(String),
    /// Nothing to hand back.
    Done,
    /// The sessions of the plugin that are still open, in start order.
    List(Vec<String>),
    /// A run of the session ended: why, and the assistant text of its last
    /// turn.
    Ended {
        /// The reason the worker named, such as `end_turn`.
        reason: String,
        /// The text the assistant wrote in that run.
        text: String,
    },
    /// The call did not happen, with the reason.
    Failed(String),
}

/// The plugins one load takes from the root.
pub(crate) enum Role {
    /// A normal run: every plugin that is not a gateway plugin, with the
    /// channel a plugin's `reagent.sessions` hands work to, when the run has a
    /// host for the sessions it starts.
    Run(Option<tokio::sync::mpsc::UnboundedSender<SessionAsk>>),
    /// A normal run a session started: every plugin that is not a gateway
    /// plugin, and no session of its own, because a session that starts
    /// sessions has no depth limit.
    Sealed,
    /// `reagent gateway`: the gateway plugins, with the channel their answers
    /// go to.
    Gateway(tokio::sync::mpsc::UnboundedSender<AnswerAsk>),
}

impl Role {
    /// Whether a plugin of role `role` is one this load takes.
    fn takes(&self, role: manifest::Role) -> bool {
        matches!(
            (self, role),
            (Role::Run(_) | Role::Sealed, manifest::Role::Normal)
                | (Role::Gateway(_), manifest::Role::Gateway)
        )
    }

    /// The channel a gateway plugin's answers go to, if this is a gateway.
    fn answers(&self) -> Option<tokio::sync::mpsc::UnboundedSender<AnswerAsk>> {
        match self {
            Role::Run(_) | Role::Sealed => None,
            Role::Gateway(answers) => Some(answers.clone()),
        }
    }

    /// The channel a plugin's sessions ask on, if this run has a host for them.
    fn sessions(&self) -> Option<tokio::sync::mpsc::UnboundedSender<SessionAsk>> {
        match self {
            Role::Run(sessions) => sessions.clone(),
            Role::Sealed | Role::Gateway(_) => None,
        }
    }

    /// Whether this run may start no session at all, which is a run a session
    /// started: its plugins load, and every `reagent.sessions` call rejects.
    fn sealed(&self) -> bool {
        matches!(self, Role::Sealed)
    }
}

/// The most jobs that may wait for the plugin thread.
const JOBS: usize = 1024;

/// The most texts `reagent.send` may queue at once; the rest are dropped.
const SENT: usize = 16;

/// The notes the plugins raised, capped so a noisy plugin cannot grow them.
#[derive(Default)]
pub(crate) struct Notes {
    /// The lines to hand back, oldest first.
    lines: Vec<String>,
    /// How many lines the cap dropped.
    dropped: u64,
    /// Woken whenever a note is raised.
    wake: Arc<Notify>,
}

impl Notes {
    /// Adds one note, or counts it as dropped when the cap is reached.
    pub(crate) fn push(&mut self, text: String) {
        if self.lines.len() < NOTES {
            self.lines.push(text);
        } else {
            self.dropped += 1;
        }
        self.wake.notify_one();
    }

    /// The signal [`Plugins::changed`] waits on.
    pub(crate) fn wake(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// Takes every note, with one line for the notes the cap dropped.
    fn drain(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.lines);
        if self.dropped > 0 {
            out.push(format!("plugins: {} more notes dropped", self.dropped));
            self.dropped = 0;
        }
        out
    }
}

/// One slash command a plugin registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    /// The name the user types after the slash.
    pub name: String,
    /// What the command does, for the host's list.
    pub description: String,
}

/// One panel a plugin made, with the lines it holds now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelView {
    /// The id clients see: `<plugin>/<id>`.
    pub id: String,
    /// What the panel is called.
    pub title: String,
    /// The lines, as one JSON array text.
    pub lines: String,
}

/// What a client sees of the panels and the texts plugins sent: the plugin
/// thread writes it, [`Plugins`] reads it.
pub(crate) struct Live {
    /// The panels, in the order they were made.
    panels: Mutex<Vec<PanelEntry>>,
    /// The texts `reagent.send` queued.
    sent: Mutex<Sent>,
    /// The notes, which a dropped send raises at once.
    notes: Arc<Mutex<Notes>>,
    /// Woken when a panel, a sent text or a note changes.
    wake: Arc<Notify>,
}

impl Live {
    /// A live state whose changes wake whoever waits on `wake`.
    pub(crate) fn new(wake: Arc<Notify>, notes: Arc<Mutex<Notes>>) -> Self {
        Self {
            panels: Mutex::new(Vec::new()),
            sent: Mutex::new(Sent::default()),
            notes,
            wake,
        }
    }

    /// Keeps a panel a plugin made, as its load committed it.
    pub(crate) fn add_panel(&self, id: String, title: String, lines: String) {
        if let Ok(mut panels) = self.panels.lock() {
            panels.push(PanelEntry {
                id,
                title,
                lines,
                changed: false,
            });
        }
    }

    /// Stores one panel's lines and marks it changed.
    pub(crate) fn publish(&self, id: &str, lines: &str) {
        let Ok(mut panels) = self.panels.lock() else {
            return;
        };
        let Some(panel) = panels.iter_mut().find(|panel| panel.id == id) else {
            return;
        };
        // Reused in place, so a burst of sets does not grow the heap.
        panel.lines.clear();
        panel.lines.push_str(lines);
        panel.changed = true;
        drop(panels);
        self.wake.notify_one();
    }

    /// Renames one panel and marks it changed.
    pub(crate) fn retitle(&self, id: &str, title: &str) {
        let Ok(mut panels) = self.panels.lock() else {
            return;
        };
        let Some(panel) = panels.iter_mut().find(|panel| panel.id == id) else {
            return;
        };
        panel.title.clear();
        panel.title.push_str(title);
        panel.changed = true;
        drop(panels);
        self.wake.notify_one();
    }

    /// Queues one text for the host; past the cap the text is dropped, and the
    /// first drop of the burst raises its one note.
    pub(crate) fn send(&self, text: String) {
        let first_drop = {
            let Ok(mut sent) = self.sent.lock() else {
                return;
            };
            if sent.texts.len() < SENT {
                sent.texts.push(text);
                false
            } else if sent.noted {
                // The burst already said so; the rest wait for `take_sent`.
                false
            } else {
                sent.noted = true;
                true
            }
        };
        if first_drop {
            // Raised here and not in `take_sent`, so a host that only drains
            // the notes still learns that texts were dropped.
            if let Ok(mut notes) = self.notes.lock() {
                notes.push(format!(
                    "plugins: {SENT} messages were waiting, the rest were dropped"
                ));
            }
        }
        self.wake.notify_one();
    }

    /// Every panel, in the order they were made.
    fn views(&self) -> Vec<PanelView> {
        self.panels
            .lock()
            .map(|panels| panels.iter().map(PanelEntry::view).collect())
            .unwrap_or_default()
    }

    /// The panels that changed since the last call, clearing their marks.
    fn take_changed(&self) -> Vec<PanelView> {
        let Ok(mut panels) = self.panels.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for panel in panels.iter_mut().filter(|panel| panel.changed) {
            panel.changed = false;
            out.push(panel.view());
        }
        out
    }

    /// The texts queued since the last call; the next burst raises its own
    /// note for what it drops.
    fn take_sent(&self) -> Vec<String> {
        let Ok(mut sent) = self.sent.lock() else {
            return Vec::new();
        };
        sent.noted = false;
        std::mem::take(&mut sent.texts)
    }
}

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Live")
            .field("panels", &self.views())
            .finish_non_exhaustive()
    }
}

/// One panel a plugin made, as the host keeps it.
struct PanelEntry {
    /// The id clients see: `<plugin>/<id>`.
    id: String,
    /// What the panel is called.
    title: String,
    /// The lines, as one JSON array text.
    lines: String,
    /// Whether it changed since the last [`Live::take_changed`].
    changed: bool,
}

impl PanelEntry {
    /// A copy for a client.
    fn view(&self) -> PanelView {
        PanelView {
            id: self.id.clone(),
            title: self.title.clone(),
            lines: self.lines.clone(),
        }
    }
}

/// The texts `reagent.send` queued, and whether the burst raised its note.
#[derive(Default)]
struct Sent {
    /// The texts, oldest first.
    texts: Vec<String>,
    /// Whether this burst already raised its one note for the dropped texts.
    noted: bool,
}

/// Every loaded plugin, and the one thread they run on.
pub struct Plugins {
    /// The plugin thread's queue.
    jobs: Sender<Job>,
    /// The notes the plugins raised, drained by [`Plugins::take_notes`].
    notes: Arc<Mutex<Notes>>,
    /// The panels and the texts the plugins sent, shared with the thread.
    live: Arc<Live>,
    /// The tools the plugins registered.
    tools: Vec<PluginTool>,
    /// The commands the plugins registered, in load order.
    commands: Vec<CommandInfo>,
    /// Whether any plugin installed a `tool.call` layer.
    call: bool,
    /// Whether any plugin installed a `tool.result` layer.
    result: bool,
    /// Whether any plugin installed a `turn.end` layer.
    turn: bool,
    /// Whether any plugin asked for the session permission, which is what a run
    /// serves sessions for.
    sessions: bool,
    /// The names of the plugins that may start sessions, in load order.
    speakers: Vec<String>,
    /// A bit per event some listener waits on.
    events: u32,
    /// How many follow-ups in a row the turn hook returned.
    follow_ups: AtomicU32,
    /// How many events the full queue dropped.
    lost_events: AtomicU64,
    /// The folder and the session every hook payload names.
    served: Arc<Mutex<Served>>,
    /// The manifest names of the plugins that loaded, in load order.
    names: Vec<String>,
}

impl Plugins {
    /// Loads every plugin under `root`; the notes say which ones failed.
    ///
    /// `root` that does not exist means no plugins, and no note. The answer
    /// is `None` when no plugin loaded. `state` is the state folder the
    /// sessions use: a plugin's cookies are saved under it. `sandbox` is the
    /// helper program every child process of a plugin starts through, which is
    /// `None` when this run has no sandbox.
    pub async fn load(
        root: &Path,
        state: &Path,
        sandbox: Option<PathBuf>,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        // The folder the run works in, which is the one a plugin's relative
        // path resolves against.
        let session = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_with(root, &session, state, sandbox, Role::Run(None)).await
    }

    /// Loads every plugin under `root`, with `sessions` the channel a plugin
    /// whose manifest grants `sessions` starts and drives sessions on.
    ///
    /// `state` and `sandbox` are as [`Plugins::load`] names them. The answer is
    /// `None` when no plugin loaded.
    pub async fn load_with_sessions(
        root: &Path,
        state: &Path,
        sandbox: Option<PathBuf>,
        sessions: tokio::sync::mpsc::UnboundedSender<SessionAsk>,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        let session = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_with(root, &session, state, sandbox, Role::Run(Some(sessions))).await
    }

    /// Loads every plugin under `root` for a run that may start no session,
    /// which is the run of a worker a session started: the plugins load as they
    /// do in the run that started it, guard hooks and all, and every
    /// `reagent.sessions` call rejects.
    ///
    /// `state` and `sandbox` are as [`Plugins::load`] names them. The answer is
    /// `None` when no plugin loaded.
    pub async fn load_sealed(
        root: &Path,
        state: &Path,
        sandbox: Option<PathBuf>,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        let session = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_with(root, &session, state, sandbox, Role::Sealed).await
    }

    /// Loads only the gateway plugins under `root`, with `answers` the channel
    /// their `reagent.gateway.answer` hands work to the caller.
    ///
    /// `state` is the state folder a plugin's cookies are saved under, and
    /// `sandbox` the helper its child processes start through. The answer is
    /// `None` when no gateway plugin loaded.
    pub async fn load_gateway(
        root: &Path,
        state: &Path,
        sandbox: Option<PathBuf>,
        answers: tokio::sync::mpsc::UnboundedSender<AnswerAsk>,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        let session = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_with(root, &session, state, sandbox, Role::Gateway(answers)).await
    }

    /// Loads every plugin under `root` with no sandbox helper, with `session`
    /// the folder a relative path of a plugin resolves against and `state` the
    /// folder its cookies are saved under.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tests load with no sandbox helper")
    )]
    pub(crate) async fn load_in(
        root: &Path,
        session: &Path,
        state: &Path,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        Self::load_with(root, session, state, None, Role::Run(None)).await
    }

    /// Loads every plugin under `root`, with `session` the folder a relative
    /// path of a plugin resolves against, `state` the folder its cookies are
    /// saved under and `sandbox` the helper every child process starts through.
    pub(crate) async fn load_with(
        root: &Path,
        session: &Path,
        state: &Path,
        sandbox: Option<PathBuf>,
        role: Role,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        let present = tokio::fs::metadata(root)
            .await
            .is_ok_and(|meta| meta.is_dir());
        if !present {
            return (None, Vec::new());
        }
        let notes = Arc::new(Mutex::new(Notes::default()));
        let served = Arc::new(Mutex::new(Served::default()));
        let Some((jobs, live)) = runtime::start(Arc::clone(&notes), Arc::clone(&served)) else {
            return (
                None,
                vec!["plugins: the plugin thread did not start".into()],
            );
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        let job = Job::Load {
            root: root.to_path_buf(),
            session: session.to_path_buf(),
            state: state.to_path_buf(),
            sandbox,
            role,
            reply,
        };
        if jobs.send(job).await.is_err() {
            return (None, drain(&notes));
        }
        let Ok(report) = answer.await else {
            return (None, drain(&notes));
        };
        if report.loaded == 0 {
            return (None, drain(&notes));
        }
        let sender = jobs.clone();
        let tools = report
            .tools
            .into_iter()
            .map(|info| {
                let ToolInfo {
                    name,
                    description,
                    parameters,
                } = info;
                PluginTool {
                    jobs: sender.clone(),
                    spec: ToolSpec {
                        name,
                        description,
                        input_schema: parameters,
                    },
                }
            })
            .collect();
        let plugins = Self {
            jobs,
            notes: Arc::clone(&notes),
            live,
            tools,
            commands: report.commands,
            call: report.call,
            result: report.result,
            turn: report.turn,
            sessions: report.sessions,
            speakers: report.speakers,
            events: report.events,
            follow_ups: AtomicU32::new(0),
            lost_events: AtomicU64::new(0),
            served,
            names: report.names,
        };
        let notes = drain(&notes);
        (Some(Arc::new(plugins)), notes)
    }

    /// The tools the plugins registered, in load order.
    pub fn tools(&self) -> Vec<PluginTool> {
        self.tools.clone()
    }

    /// Names the run the plugins serve: its folder, which is absolute, and its
    /// session, which a run that keeps none leaves out.
    ///
    /// Every payload a `tool.call` or `tool.result` hook gets names both.
    pub fn serving(&self, folder: &Path, session: Option<&str>) {
        if let Ok(mut served) = self.served.lock() {
            served.name(folder, session);
        }
    }

    /// The manifest names of the plugins that loaded, in load order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The commands, in load order.
    pub fn commands(&self) -> &[CommandInfo] {
        &self.commands
    }

    /// Runs one command; the text to show, if any.
    ///
    /// An unknown name, a throw or a stop gives a text that names the plugin
    /// (when known), the command and the error.
    pub async fn command(&self, name: &str, args: &str) -> Option<String> {
        if !self.commands.iter().any(|command| command.name == name) {
            return Some(format!("plugins: no command is named '/{name}'"));
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        let job = Job::Command {
            name: name.to_string(),
            args: args.to_string(),
            reply,
        };
        if self.jobs.send(job).await.is_err() {
            return Some("plugins: the plugin runtime stopped".into());
        }
        match answer.await {
            Ok(text) => text,
            Err(_) => Some("plugins: the plugin runtime stopped".into()),
        }
    }

    /// Every panel with its current lines, in the order the panels were made,
    /// for a client that attaches.
    pub fn panels(&self) -> Vec<PanelView> {
        self.live.views()
    }

    /// The panels whose lines or title changed since the last call; clears the
    /// marks.
    pub fn take_changed_panels(&self) -> Vec<PanelView> {
        self.live.take_changed()
    }

    /// The messages `reagent.send` queued since the last call.
    pub fn take_sent(&self) -> Vec<String> {
        self.live.take_sent()
    }

    /// Resolves when a panel changed, a message was sent or a note was raised
    /// after the last call of the matching `take_*`. Cancel safe.
    pub async fn changed(&self) {
        self.live.wake.notified().await;
    }

    /// Whether any plugin listens on the event `name`; false for a name no
    /// plugin may listen on.
    ///
    /// A host asks this before it builds an event's payload: a run whose plugins
    /// listen on nothing costs no allocation for it.
    pub fn wants(&self, name: &str) -> bool {
        api::event_bit(name).is_some_and(|bit| self.events & bit != 0)
    }

    /// Hands one event to every listener waiting on it; it never blocks.
    ///
    /// An event the full queue cannot take is dropped and counted into one
    /// note, so a fire-hosing plugin costs the run no memory.
    pub fn event(&self, name: &str, payload: JsonValue) {
        if !self.wants(name) {
            return;
        }
        let job = Job::Event {
            name: name.to_string(),
            payload,
        };
        if self.jobs.try_send(job).is_err() {
            self.lost_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether any plugin may start sessions, which is what makes a run serve
    /// the sessions its plugins start.
    pub fn sessions(&self) -> bool {
        self.sessions
    }

    /// The names of the plugins that may start sessions, in load order, which
    /// are the only ones a run starts a session for.
    pub fn speakers(&self) -> Vec<String> {
        self.speakers.clone()
    }

    /// Hands one frame of the session `id` to the plugin `plugin` that started
    /// it: only its own listeners see it, through its own `on`.
    ///
    /// A frame the full queue cannot take is dropped and counted into one note,
    /// like an event.
    pub fn session_event(&self, plugin: &str, id: &str, event: JsonValue) {
        let job = Job::Session {
            plugin: plugin.to_owned(),
            id: id.to_owned(),
            event,
        };
        if self.jobs.try_send(job).is_err() {
            self.lost_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Takes the notes raised since the last call, such as a stopped hook.
    pub fn take_notes(&self) -> Vec<String> {
        let mut out = drain(&self.notes);
        let lost = self.lost_events.swap(0, Ordering::Relaxed);
        if lost > 0 {
            out.push(format!(
                "plugins: {lost} events dropped, the queue was full"
            ));
        }
        out
    }

    /// Adds one note.
    fn note(&self, text: String) {
        if let Ok(mut notes) = self.notes.lock() {
            notes.push(text);
        }
    }
}

impl fmt::Debug for Plugins {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugins")
            .field("tools", &self.tools)
            .field("commands", &self.commands)
            .field("call", &self.call)
            .field("result", &self.result)
            .field("turn", &self.turn)
            .field("events", &self.events)
            .finish_non_exhaustive()
    }
}

/// One tool a plugin registered, offered to the model like a native tool.
#[derive(Clone)]
pub struct PluginTool {
    /// The plugin thread's queue.
    jobs: Sender<Job>,
    /// The spec the model sees.
    spec: ToolSpec,
}

impl fmt::Debug for PluginTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginTool")
            .field("name", &self.spec.name)
            .finish()
    }
}

impl Tool for PluginTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let job = Job::Run {
                tool: self.spec.name.clone(),
                args: args.to_string(),
                reply,
            };
            if self.jobs.send(job).await.is_err() {
                return ToolOutput::error("the plugin runtime stopped");
            }
            let out = tokio::select! {
                out = answer => out,
                () = cancel.cancelled() => return ToolOutput::error("cancelled"),
            };
            match out {
                Ok(RunReply { text, is_error }) => {
                    let mut output = ToolOutput::ok(text);
                    output.is_error = is_error;
                    output
                }
                Err(_) => ToolOutput::error("the plugin runtime stopped"),
            }
        })
    }
}

impl CallHook for Plugins {
    fn before<'a>(&'a self, tool: &'a str, args: &'a str) -> BoxFuture<'a, CallVerdict> {
        if !self.call {
            return Box::pin(std::future::ready(CallVerdict::Keep));
        }
        Box::pin(async move {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let job = Job::Call {
                tool: tool.to_string(),
                args: args.to_string(),
                reply,
            };
            if self.jobs.send(job).await.is_err() {
                return CallVerdict::Keep;
            }
            // Each layer ends within its own limit, so the chain is bounded by
            // the sum of them and no cap of its own cuts a layer short.
            match answer.await {
                Ok(CallReply::Args(args)) => CallVerdict::Args(args),
                Ok(CallReply::Block(reason)) => CallVerdict::Block(reason),
                _ => CallVerdict::Keep,
            }
        })
    }

    fn after<'a>(
        &'a self,
        tool: &'a str,
        args: &'a str,
        output: ToolOutput,
    ) -> BoxFuture<'a, ToolOutput> {
        if !self.result {
            return Box::pin(std::future::ready(output));
        }
        Box::pin(async move {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let job = Job::Result {
                tool: tool.to_string(),
                args: args.to_string(),
                text: output.content.to_string(),
                is_error: output.is_error,
                reply,
            };
            if self.jobs.send(job).await.is_err() {
                return output;
            }
            match tokio::time::timeout(CHAIN_TIME, answer).await {
                Ok(Ok(reply)) => {
                    // The layers see only the text; everything else the call
                    // produced stays as it is.
                    let mut output = output;
                    if let Some(text) = reply.text {
                        output.content = text.into();
                    }
                    if let Some(is_error) = reply.is_error {
                        output.is_error = is_error;
                    }
                    output
                }
                Ok(Err(_)) => output,
                Err(_) => {
                    self.note("plugins: the tool.result chain timed out after 30 s".into());
                    output
                }
            }
        })
    }
}

impl TurnHook for Plugins {
    fn turn_end<'a>(&'a self, turn: &'a TurnView<'a>) -> BoxFuture<'a, Option<Text>> {
        if !self.turn {
            return Box::pin(std::future::ready(None));
        }
        Box::pin(async move {
            let text = reply_text(turn);
            let (reply, answer) = tokio::sync::oneshot::channel();
            let job = Job::Turn {
                index: turn.index,
                ran_tools: turn.ran_tools,
                text,
                reply,
            };
            if self.jobs.send(job).await.is_err() {
                return None;
            }
            let follow = match tokio::time::timeout(CHAIN_TIME, answer).await {
                Ok(Ok(follow)) => follow.join("\n\n"),
                Ok(Err(_)) => String::new(),
                Err(_) => {
                    self.note("plugins: the turn.end chain timed out after 30 s".into());
                    String::new()
                }
            };
            if follow.is_empty() {
                self.follow_ups.store(0, Ordering::Relaxed);
                return None;
            }
            let done = self.follow_ups.load(Ordering::Relaxed);
            if done >= FOLLOW_UPS {
                self.note(format!(
                    "plugins: {FOLLOW_UPS} follow-ups in a row, the turn ends"
                ));
                return None;
            }
            self.follow_ups.store(done + 1, Ordering::Relaxed);
            Some(Text::from(follow))
        })
    }
}

/// The visible text of the turn's assistant reply.
fn reply_text(turn: &TurnView<'_>) -> String {
    let mut text = String::new();
    for content in &turn.assistant.content {
        if let Content::Text { text: part } = content {
            text.push_str(part);
        }
    }
    text
}

/// Takes every note out of `notes`.
fn drain(notes: &Mutex<Notes>) -> Vec<String> {
    notes
        .lock()
        .map(|mut notes| notes.drain())
        .unwrap_or_default()
}
