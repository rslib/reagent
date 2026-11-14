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

use runtime::{CallReply, Job, RunReply, ToolInfo};

/// The most times in a row a plugin may add a follow-up.
const FOLLOW_UPS: u32 = 3;

/// The longest a hook chain may take before its answer is treated as keep.
const CHAIN_TIME: Duration = Duration::from_secs(30);

/// The most notes kept at once; the rest are counted, not stored.
const NOTES: usize = 100;

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
    /// A bit per event some listener waits on.
    events: u8,
    /// How many follow-ups in a row the turn hook returned.
    follow_ups: AtomicU32,
    /// How many events the full queue dropped.
    lost_events: AtomicU64,
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
        Self::load_with(root, &session, state, sandbox).await
    }

    /// Loads every plugin under `root` with no sandbox helper, with `session`
    /// the folder a relative path of a plugin resolves against and `state` the
    /// folder its cookies are saved under.
    #[cfg_attr(
        not(all(test, feature = "node")),
        expect(
            dead_code,
            reason = "the node module tests load with no sandbox helper"
        )
    )]
    pub(crate) async fn load_in(
        root: &Path,
        session: &Path,
        state: &Path,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        Self::load_with(root, session, state, None).await
    }

    /// Loads every plugin under `root`, with `session` the folder a relative
    /// path of a plugin resolves against, `state` the folder its cookies are
    /// saved under and `sandbox` the helper every child process starts through.
    pub(crate) async fn load_with(
        root: &Path,
        session: &Path,
        state: &Path,
        sandbox: Option<PathBuf>,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        let present = tokio::fs::metadata(root)
            .await
            .is_ok_and(|meta| meta.is_dir());
        if !present {
            return (None, Vec::new());
        }
        let notes = Arc::new(Mutex::new(Notes::default()));
        let Some((jobs, live)) = runtime::start(Arc::clone(&notes)) else {
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
            events: report.events,
            follow_ups: AtomicU32::new(0),
            lost_events: AtomicU64::new(0),
        };
        let notes = drain(&notes);
        (Some(Arc::new(plugins)), notes)
    }

    /// The tools the plugins registered, in load order.
    pub fn tools(&self) -> Vec<PluginTool> {
        self.tools.clone()
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

    /// Hands one event to every listener waiting on it; it never blocks.
    ///
    /// An event the full queue cannot take is dropped and counted into one
    /// note, so a fire-hosing plugin costs the run no memory.
    pub fn event(&self, name: &str, payload: JsonValue) {
        let Some(bit) = api::event_bit(name) else {
            return;
        };
        if self.events & bit == 0 {
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
            match tokio::time::timeout(CHAIN_TIME, answer).await {
                Ok(Ok(CallReply::Args(args))) => CallVerdict::Args(args),
                Ok(Ok(CallReply::Block(reason))) => CallVerdict::Block(reason),
                Ok(Ok(CallReply::Keep)) => CallVerdict::Keep,
                Ok(Err(_)) => CallVerdict::Keep,
                Err(_) => {
                    self.note("plugins: the tool.call chain timed out after 30 s".into());
                    CallVerdict::Keep
                }
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
